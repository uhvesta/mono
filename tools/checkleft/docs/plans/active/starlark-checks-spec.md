# Checkleft Starlark Checks Spec

**Status:** Draft
**Date:** 2026-06-24

---

## 1. Goals

1. Let users define **evolution checks** (proto, JSON schema, Java API surface, etc.) in typed Starlark with minimal Rust involvement.
2. Opinionated folder structure: one directory = one check. Shared helpers live alongside checks and are importable.
3. Sandbox tiers declared in `check_meta()` — hermetic by default, opt-in network access.
4. Versioned check distribution — pull in third-party or org-published check packages at pinned versions.
5. Optional **fix** functions co-located with checks.
6. Bidirectional: checks can be authored in Starlark _or_ Rust. Rust checks and Starlark checks share the same output types and runner pipeline.
7. Hierarchical: repos can define checks at the root; sub-projects can layer on their own.
8. Maximal Starlark typing via `DialectTypes::Enable` — all function signatures, parameters, and return types must carry type annotations.
9. **Version sets** — curated bundles of exact package pins that consumers can adopt as a single dependency, inspired by Amazon Brazil version sets.

---

## 2. Folder structure

### 2.1 Package directories

Any directory containing a `checkleft-package.toml` file is a check package. The directory name is not significant — `checkleft-package.toml` is the discovery marker.

```
repo-root/
├── checks/
│   ├── checkleft-package.toml             # package manifest (required)
│   ├── lib/                               # shared helper modules
│   │   ├── matchers.checkleft
│   │   └── proto_helpers.checkleft
│   ├── proto/                             # adapter = proto
│   │   ├── evolution/
│   │   │   ├── check.checkleft
│   │   │   └── fix.checkleft
│   │   └── team_policy/
│   │       └── check.checkleft
│   ├── module_json/                       # adapter = module_json
│   │   └── required_fields/
│   │       └── check.checkleft
│   └── java/                              # adapter = java
│       └── api_stability/
│           ├── check.checkleft
│           └── fix.checkleft
├── services/
│   └── payments/
│       └── billing-checks/                # nested project-level checks
│           ├── checkleft-package.toml
│           └── proto/
│               └── billing_compat/
│                   └── check.checkleft
```

### 2.2 Design philosophy: one way to do things

This folder structure is **intentionally opinionated**. There is exactly one way to determine each property of a check:

- **Which adapter?** → look at the first-level folder (`proto/`, `module_json/`, etc.)
- **Which check?** → look at the remaining path under the adapter folder
- **What files does it run on?** → the adapter's file selectors, narrowed by `CHECKS.yaml`
- **Which files does this repo choose to validate?** → read `CHECKS.yaml`
- **Where are shared helpers?** → `lib/`
- **Where are external package/version-set pins?** → `CHECKS.yaml`
- **Where is the package root?** → the directory containing `checkleft-package.toml`

No config flags, no overrides, no implicit conventions in package source. The directory tree is the source of truth for check identity and author tests; `CHECKS.yaml` is the source of truth for consumer activation and path policy.

### 2.3 Rules

| Path pattern (relative to package root)    | Role                                                                                                                    |
| ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| `checkleft-package.toml`                   | **Package manifest.** Declares producer metadata and publishing/version-set metadata. Required. Presence marks the directory as a package root. |
| `lib/*.checkleft`                          | **Shared modules.** Importable helpers for checks in the same package.                                                  |
| `<adapter>/<name>/check.checkleft`         | **Check definition.** Exported as part of the package API when published.                                               |
| `<adapter>/<name>/fix.checkleft`           | **Fix definition.** Optional. Must export a `fix()` function.                                                           |
| `<adapter>/<name>/testdata/<case>/`        | **Functional tests.** Fixture-based test cases discovered by path. See §13.                                             |
| `<adapter>/<name>/*.checkleft`             | **Check-local helpers.** Any other `.checkleft` file is a local helper, loadable only from within that check directory. |

The path structure is: `<adapter>/<name>`.

- **`<adapter>`** — selects the Rust format adapter (e.g. `proto`, `module_json`, `java`, `text`). Must match a registered `FormatAdapter::kind()`. This is the structural guarantee that every check under `proto/` uses the proto adapter and receives a `ProtoEvolutionContext`.
- **`<name>`** — the check name. Can be nested (e.g. `evolution/deletions`). Forms the check ID as `<adapter>/<name>` (e.g. `proto/evolution` or `proto/evolution/deletions`).

Checks can be nested under a parent to create logical groupings:

```
proto/
├── evolution/
│   ├── check.checkleft              # check ID: proto/evolution
│   ├── deletions/
│   │   └── check.checkleft          # check ID: proto/evolution/deletions
│   └── field_numbering/
│       └── check.checkleft          # check ID: proto/evolution/field_numbering
└── naming/
    └── check.checkleft              # check ID: proto/naming
```

Each directory with a `check.checkleft` is an independent check. Nesting is purely organizational — a parent check does not compose or invoke its children.

**Enforcement:**

- A directory containing `check.checkleft` must be at least two levels deep under the package root (adapter + name, with name being one or more levels).
- The first level must match a registered adapter. Unknown adapter names are an error at discovery time.
- `checkleft-package.toml` must exist at the package root. Without it, the directory is not a package.
- File extension is always `.checkleft`. No `.star`, `.bzl`, or `.py`.

### 2.4 Producer/consumer model

**Every directory with a `checkleft-package.toml` is a package.** The directory name does not matter — only the presence of the manifest file.

**A check in a published package is part of that package's API.** If a check should not be visible to consumers, keep it out of the published package root or keep it in a local package that is not selected by consumer policy.

**Cross-package consumption — even within the same monorepo — goes through `CHECKS.yaml`.** A consumer selects packages, version sets, and local path packages in validation policy. `checkleft-package.toml` never decides what another repo runs.

**Key principles:**

- `checkleft-package.toml` is producer metadata, not consumer validation policy.
- Check source paths define reusable check IDs and same-package helper loading.
- `CHECKS.yaml` decides what runs in a consumer repo, on which paths, with which policy.
- Version sets are curated bundles: selecting a version set activates all checks from all packages it includes.

---

## 3. Package metadata, check layout, and activation

The Starlark package system deliberately separates producer concerns from consumer validation concerns:

- `checkleft-package.toml` answers "what package is this, and how is it published?"
- Check/fix/lib paths answer "what reusable checks does this package define?"
- `CHECKS.yaml` answers "which packages/version sets run here, against which files, with which policy?"

### 3.1 `checkleft-package.toml` — producer metadata

Written in TOML. Parsed before publishing, package testing, and package loading. It declares package identity and publishing metadata. It does **not** declare consumer validation policy, repo path scope, excludes, or which checks should run.

```toml
# checks/checkleft-package.toml

[package]
name = "myorg/repo-checks"
version = "0.1.0"
kind = "check_package"

[publish]
description = "Repository policy checks for myorg"
license = "Apache-2.0"
# Consumed during `checkleft publish` and registry upload. Ignored at check-run time.
```

#### `[package]` fields

| Field     | Type  | Required | Description                                                                                 |
| --------- | ----- | -------- | ------------------------------------------------------------------------------------------- |
| `name`    | `str` | yes      | Globally unique package name. Convention: `<org>/<descriptor>`.                             |
| `version` | `str` | yes      | SemVer package version. Used by consumers and version sets when pinning this package.        |
| `kind`    | `str` | no       | `check_package` (default) or `version_set`.                                                  |

`checkleft-package.toml` intentionally has no `exclude`, no consumer `[dependencies]`, and no "activate these checks" section. Those belong in `CHECKS.yaml`.

### 3.2 Check package layout

Local check files are auto-discovered from the package folder structure. They are **not** listed in `checkleft-package.toml`.

```
my-checks/
├── checkleft-package.toml
├── lib/
│   └── proto_helpers.checkleft
├── proto/
│   └── evolution/
│       ├── check.checkleft
│       ├── fix.checkleft
│       └── testdata/
│           └── removes_field/
│               ├── before/
│               ├── after/
│               └── expected.toml
└── text/
    └── no_debug/
        └── check.checkleft
```

- Any directory matching `<adapter>/<name>/` (relative to the package root) that contains a `check.checkleft` is a check.
- The check ID is derived from the path: `<adapter>/<name>` (for example, `proto/evolution`).
- A check in a published package is part of that package's API. Experiments that should not be part of a package API belong outside the published package root or in an unpublished local package.
- The adapter's file selectors determine which files the check inspects. Consumer policy can further narrow the target set in `CHECKS.yaml`.
- `lib/*.checkleft` modules are same-package helpers. Checks inside the package can `load("//lib/foo", ...)`; consumers cannot import package libs directly.

### 3.3 Version-set packages

A version set is a separate package whose `checkleft-package.toml` has `kind = "version_set"`. It pins a curated set of exact package refs and hashes.
`[includes.<name>]` tables are only valid in version-set manifests. A `check_package` manifest does not declare dependencies or included packages; consumers select packages in `CHECKS.yaml`.

```toml
# Published as: acme-versionset v2025.06.1

[package]
name = "acme/versionset"
version = "2025.06.1"
kind = "version_set"

[includes.proto_evolution]
source = "registry://checkleft-hub/proto-evolution"
version = "0.2.1"
sha256 = "14c6000000000000000000000000000000000000000000000000000000000000"

[includes.module_json_checks]
source = "registry://checkleft-hub/module-json"
version = "1.3.0"
sha256 = "f041000000000000000000000000000000000000000000000000000000000000"

[includes.java_api_compat]
source = "registry://checkleft-hub/java-api-compat"
version = "0.8.2"
sha256 = "827a000000000000000000000000000000000000000000000000000000000000"

[includes.security_baseline]
source = "registry://checkleft-hub/security-baseline"
version = "3.1.0"
sha256 = "e91d000000000000000000000000000000000000000000000000000000000000"
```

Selecting a version set in `CHECKS.yaml` activates all checks from all included packages. A version set is therefore a curated API surface: adding, removing, or renaming a check is a meaningful version-set change.

#### `[includes.<name>]` fields

| TOML key    | Type  | Required | Description                                                 |
| ----------- | ----- | -------- | ----------------------------------------------------------- |
| (table key) | `str` | yes      | Local alias for this constituent package.                   |
| `source`    | `str` | yes      | Source URI of the constituent package.                      |
| `version`   | `str` | yes      | Exact version pin. The version set author tests this pin.   |
| `sha256`    | `str` | yes      | Canonical SHA-256 digest of the published constituent package bytes: 64 lowercase hex characters. |

### 3.4 `CHECKS.yaml` — consumer validation policy

Consumers select validation policy in `CHECKS.yaml`, alongside existing built-in/declarative check config. This keeps "what runs here?" reviewable in the file that already owns checkleft policy.

```yaml
checkleft_packages:
  version_sets:
    - source: registry://checkleft-hub/acme-versionset
      version: "2025.06.1"
      sha256: "b3d1000000000000000000000000000000000000000000000000000000000000"

  packages:
    - source: git://github.com/myteam/checkleft-checks.git
      version: "0.3.0"
      sha256: "9f200000000000000000000000000000000000000000000000000000000000"

    - source: path://tools/my-checks
      version: "0.0.0-local"
      mode: explicit

checks:
  # Version sets activate all checks from their included packages.
  # This entry narrows the file scope for one activated check in this repo.
  - id: proto/evolution
    include:
      - "api/**/*.proto"
    exclude:
      - "api/generated/**"

  # Local explicit packages do not auto-activate; opt in check-by-check.
  - id: local_experiments:text/no_debug
    include:
      - "**/*.txt"
```

`checkleft_packages.version_sets` entries activate every check in every package listed by the selected version set. `checkleft_packages.packages` entries can opt into `mode: all` or `mode: explicit`; local path packages default to `explicit` for safe iteration, while fetched packages default to `all`.

Path selection is two-stage:

1. The adapter's file selectors determine which files are relevant (e.g. `*.proto` for the proto adapter).
2. `CHECKS.yaml` `include`/`exclude` narrows the effective file set for this repo.

`CHECKS.yaml` controls which checks run and on which paths — it does not configure check behavior.

### 3.5 Resolution rules

A consumer activates exactly the version sets and packages selected in `CHECKS.yaml`.

1. **Every external ref is exact and hash-pinned.** The resolver fetches package bytes for `source`/`version` and fails closed unless the bytes match `sha256`. `sha256` values are canonical lowercase 64-hex digests; placeholder or mixed-case values are rejected at parse time.
2. **Version sets are curated package bundles.** A version set package contains `[package]` metadata and `[includes.*]` entries. It does not define checks of its own and it cannot depend on another version set.
3. **A version set's `sha256` covers the version-set package.** The package contains the exact ordered constituent `(source, version, sha256)` refs, so changing any included package changes the version-set package hash.
4. **No transitive dependency closure is loaded.** Packages do not activate other packages. Checks run only from selected packages or packages included by selected version sets.
5. **Duplicate package names are a hard error unless they are byte-identical.** If two selected refs name the same package with different `source`/`version`/`sha256`, resolution fails and the consumer must choose one.
6. **Version sets activate all checks from included packages.** Consumers control the selected version-set version; the version-set author controls the check set.
7. **Individual packages can be activated in `all` or `explicit` mode.** Version sets are always `all`.

### 3.6 Self-hosted guard check for policy integrity

Organizations that want to prevent removal or downgrade of required policy can supply an external root `CHECKS.yaml` through the existing external config mechanism. That root config enables a Starlark guard check targeting `CHECKS.yaml` / `CHECKS.toml`.

Initial guard behavior:

- Compare the base and current config files.
- Fail if a selected version set or package pin is downgraded.
- Fail if a hardcoded protected version set or package entry is removed.
- Use hardcoded placeholder protected entries to prove the policy-check execution path.

This keeps the platform rule explicit: the guard is just another Starlark check supplied by org policy, not hidden behavior in package resolution.

---

## 4. Check and fix entry points

### 4.1 `check.checkleft` — the check file

Every check file must:

1. Call `check_meta()` at the top level to declare metadata.
2. Define exactly one `check()` function with a typed signature. The parameter type depends on the **file format adapter** (see §6).

The adapter determines which files the check receives — not the check itself. A check under `proto/` automatically receives all files matching the proto adapter's file selectors. The check author's only declarations are the sandbox tier and the `check()` function.

```python
# proto/evolution/check.checkleft

load("//lib/proto_helpers", "is_reserved")

check_meta(
    tier = "hermetic",
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

| Field  | Type  | Required | Default      | Description           |
| ------ | ----- | -------- | ------------ | --------------------- |
| `tier` | `str` | no       | `"hermetic"` | Sandbox tier. See §5. |

### 4.2 `fix.checkleft` — the fix file

Optional. Must define a `fix()` function whose signature mirrors the check but returns `list[FileEdit]`.

**Invariant:** The check and fix interact through **`fix_data`** — a strongly typed struct attached to each finding. This is the only mechanism for passing computed data from a check to its fix. Because the check and fix live in the same directory, they share a type definition via a local helper. The runtime validates `fix_data` against its declared type, so malformed data is caught at check evaluation time, not silently passed to the fix. Type mismatches are caught by `checkleft_test` and `checkleft_package` Bazel rules.

A finding with `fix_data = None` means "not auto-fixable" — the fix skips it. If a check has a sibling `fix.checkleft`, auto-fixable findings **must** carry typed `fix_data`.

**Step 1: Define a shared `fix_data` type in a check-local helper.**

```python
# checkleft/proto/evolution/types.checkleft

# Each fix_data variant is a struct with typed fields.
# The check constructs these; the fix pattern-matches on them.

def field_not_reserved(field_number: int, field_name: str, insertion_line: int) -> FieldNotReserved:
    return FieldNotReserved(
        field_number = field_number,
        field_name = field_name,
        insertion_line = insertion_line,
    )

FieldNotReserved = struct(
    field_number = int,
    field_name = str,
    insertion_line = int,
)
```

**Step 2: The check constructs typed `fix_data`.**

```python
# checkleft/proto/evolution/check.checkleft

load(":types", "field_not_reserved")

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []
    for delta in ctx.deltas:
        if delta.kind == DeltaKind.field_removed:
            findings.append(finding(
                severity = Severity.fail,
                message = "removed field {} must be reserved".format(delta.symbol),
                path = delta.path,
                fix_data = field_not_reserved(
                    field_number = delta.before_number,
                    field_name = delta.symbol.split(".")[-1],
                    insertion_line = delta.line,
                ),
            ))
    return findings
```

**Step 3: The fix reads typed `fix_data` — no string parsing, no dict key guessing.**

```python
# checkleft/proto/evolution/fix.checkleft

load(":types", "FieldNotReserved")

def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    edits: list[FileEdit] = []
    for f in findings:
        if f.fix_data == None:
            continue
        if type(f.fix_data) == FieldNotReserved:
            edits.append(file_edit(
                path = f.path,
                old_text = "",
                new_text = "  reserved {};\n  reserved \"{}\";\n".format(
                    f.fix_data.field_number,
                    f.fix_data.field_name,
                ),
                after_line = f.fix_data.insertion_line,
            ))
    return edits
```

**Type safety guarantees:**

- `fix_data` is typed as `struct | None` on `Finding`. The Starlark type checker validates that the check passes a struct, not an arbitrary dict.
- The fix does `type(f.fix_data) == FieldNotReserved` for runtime dispatch — this is a real type check, not string matching.
- Both check and fix `load()` from the same `:types` module, so they share the struct definition. A field rename or type change in the struct is caught by the type checker on both sides.
- Findings without `fix_data` (set to `None`) are simply not auto-fixable. The fix skips them.

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
load(":types", "field_not_reserved")
```

| Prefix | Resolution                                        |
| ------ | ------------------------------------------------- |
| `//`   | Relative to the package root (the directory containing `checkleft-package.toml`). |
| `:`    | Relative to the current check directory.          |

External dependencies provide **checks only** — their `lib/` modules and internal helpers are never loadable by consumers. There is no `@<dep_name>//` load path. Dependencies are consumed as opaque check packages, not as importable libraries.

---

## 5. Sandbox tiers

Checks declare their required sandbox tier in `check_meta()` inside `check.checkleft`. The tier determines what host capabilities the Starlark environment exposes.

### 5.1 Tier definitions

| Tier         | ID           | Capabilities                                                                                                                                                                        | Use case                                                                                      |
| ------------ | ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| **Hermetic** | `"hermetic"` | Typed data models (injected by the adapter), pure computation, `load()`. No file I/O, no network, no subprocesses. Starlark never sees raw file contents or touches the filesystem. | Most evolution checks. Default.                                                               |
| **Network**  | `"network"`  | Everything in hermetic + HTTP GET requests via `http_get()` built-in. DNS resolution allowed. Still no file I/O or arbitrary exec.                                                  | Checks that validate against a remote reservation service, API catalog, or internal registry. |

### 5.2 Tier enforcement

- Starlark code **never has filesystem access**. All data arrives as typed models injected by the Rust adapter. There is no `read_file()`, `open()`, or equivalent. The Starlark environment is a pure computation sandbox over pre-parsed data.
- The Starlark `Globals` environment is constructed per-tier. Hermetic checks simply never see `http_get` or similar symbols — they don't exist in scope, so misuse is a compile-time name-resolution error, not a runtime denial.
- **Tier escalation is forbidden.** A check declared `hermetic` cannot `load()` a module that was authored for `network` tier. Tier is a property of the check's `check_meta()` declaration, not of individual loaded `.checkleft` files. All code reachable from a check runs at that check's tier.
- CI environments may **deny the `network` tier entirely** via a runner flag (`--deny-tier=network`). Checks activated at a denied tier are skipped with a warning.

### 5.3 Severity model

Findings have exactly two severity levels. A finding always blocks merge — the only question is whether a human can override it.

| Severity                 | Starlark constant               | CI behavior                                                                                            | Description                                                                |
| ------------------------ | ------------------------------- | ------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| **Fail**                 | `Severity.fail`                 | Blocks merge. Cannot be overridden.                                                                    | Hard violation. No exceptions.                                             |
| **Fail-but-overridable** | `Severity.fail_but_overridable` | Blocks merge by default, but can be overridden through the repository's approved override mechanism. | The change is almost certainly wrong, but there are legitimate exceptions. |

There is no "informational" / "notice" severity. If something doesn't warrant blocking the build, it doesn't belong as a checkleft finding — use linter warnings or comments for that. Checkleft findings are gates.

Severity is set per-finding by the check author. The system can only **escalate** severity (`fail_but_overridable` → `fail`), never **relax** it. This ensures check authors set the floor.

Shorthand constructors match the severity names:

```python
fail(message = "...", path = "...")                   # Severity.fail
fail_but_overridable(message = "...", path = "...")   # Severity.fail_but_overridable
```

### 5.4 Diagnostic display model

A finding is an inline diagnostic. The check author controls how precise it is:

- `path` only: file-level diagnostic.
- `path` + `line`: line-level diagnostic.
- `path` + `line` + `column`: point diagnostic.
- `path` + `line` + `column` + `end_line` + `end_column`: span diagnostic.

The UI should render the most precise location present and gracefully degrade when a check only has file-level context. The `message` field is the primary human-facing diagnostic text; it may include both what is wrong and how to remediate it. The optional `remediation` field is secondary structured guidance for checks that want the UI to render the suggested action separately.

`message` and `remediation` are intentionally separate from `fix_data`: they are for humans, while `fix_data` is typed machine data consumed only by `fix.checkleft`.

### 5.5 Tier-specific built-in bindings

**Hermetic tier** (always available):

| Symbol                       | Type                               | Description                                             |
| ---------------------------- | ---------------------------------- | ------------------------------------------------------- |
| `finding(...)`               | `fn(...) -> Finding`               | Construct a finding.                                    |
| `file_edit(...)`             | `fn(...) -> FileEdit`              | Construct a file edit (for fixes).                      |
| `Severity`                   | `enum{fail, fail_but_overridable}` | Severity constants (see §5.3).                          |
| `DeltaKind`                  | adapter-specific enum               | Each adapter injects its own variants (e.g., `DeltaKind.field_removed` for proto, `DeltaKind.method_removed` for java). Only the variants for the check's adapter are in scope. |
| `print(...)`                 | `fn(str)`                          | Debug print (suppressed in CI, shown with `--verbose`). |
| `regex_match(pattern, s)`    | `fn(str, str) -> bool`             | RE2 regex match.                                        |
| `regex_find_all(pattern, s)` | `fn(str, str) -> list[str]`        | RE2 find all matches.                                   |
| `glob_match(pattern, path)`  | `fn(str, str) -> bool`             | Glob pattern match.                                     |

**Network tier** (additional bindings):

| Symbol                                         | Type                                                     | Description                           |
| ---------------------------------------------- | -------------------------------------------------------- | ------------------------------------- |
| `http_get(url, headers=None, timeout_ms=5000)` | `fn(str, dict[str,str]\|None, int) -> HttpResponse`      | HTTP GET. TLS required. No mutations. |
| `HttpResponse`                                 | `struct{status: int, body: str, headers: dict[str,str]}` | Response type.                        |

---

## 6. File format adapters — the Rust/Starlark bridge

Evolution checks need **parsed representations** of files at two revisions (base and current). Parsing binary formats (protobuf descriptors, Java class files, JSON schemas) in pure Starlark is impractical and slow. This is where Rust earns its keep.

### 6.1 Adapter architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Rust host                             │
│                                                         │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐ │
│  │ ProtoAdapter  │   │ ModuleJson   │   │ JavaAdapter  │ │
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
        changeset: &ChangeSet,
    ) -> Result<()>;

    /// Return the Starlark type name for the context parameter (e.g. "ProtoEvolutionContext").
    fn context_type_name(&self) -> &str;
}
```

### 6.2 Built-in adapters

#### `proto` — Protobuf evolution

Context type: `ProtoEvolutionContext`

Rust side: asks the repository's descriptor provider/native proto path for source-info-rich `FileDescriptorSet` values at both base and current revisions. The adapter must not directly invoke `protoc`; descriptor generation belongs to the existing native descriptor integration. The resulting `FileDescriptorSet` is enriched with source location info (comments, line/column positions for every element). The Rust adapter parses these descriptor sets, diffs them into `SchemaDelta` values, and injects the typed models into Starlark. Starlark check authors receive a descriptor model that includes comments and source positions — not raw `.proto` text, but the full structured descriptor representation.

Starlark surface: `ctx.deltas`, `ctx.files`, plus all the typed descriptor types (`FileDescriptor`, `MessageDescriptor`, `FieldDescriptor`, etc.) and enum constants (`DeltaKind`, `FieldKind`, `FieldLabel`, etc.) already documented in the proto-evolution branch. Source location info is available on descriptors via `.source_location` (line, column, leading/trailing comments).

**Vendored extensions:** The proto adapter makes a set of well-known extension `.proto` files (e.g. org-wide custom options) available to the descriptor provider. Custom options defined in these vendored protos are resolved in every descriptor set automatically — no user configuration needed. Checks can inspect them via `msg.options.extensions`.

#### `module_json` — `module.json` file evolution

Context type: `ModuleJsonEvolutionContext`

Adapters are **not** generic file-format parsers. They are specific to concrete file types with their own schemas and evolution semantics. The `module_json` adapter understands `module.json` files — their required keys, dependency structure, and versioning semantics. A different JSON-based file type (e.g. `package.json`, `tsconfig.json`) would get its own adapter with its own typed model.

Rust side: parses `module.json` files at both revisions into a typed `ModuleJson` model (not generic JSON). Computes structural diffs aware of module-specific semantics (e.g. dependency additions vs. removals, version field changes).

```python
# Starlark surface
ctx.files       # list[ModuleJsonFilePair]  — before/after parsed module.json
ctx.deltas      # list[ModuleJsonDelta]     — structured changes

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

`text` is a registered adapter like any other. Checks under `text/` use the text adapter. There is no implicit fallback — an unrecognized adapter folder name is an error at discovery time (see §2.3).

### 6.4 Registering custom Rust adapters

Third-party or in-repo Rust adapters register via:

```rust
// In the host binary's setup
registry.register_adapter(Box::new(MyCustomAdapter));
```

The adapter's `kind()` return value must match the top-level `<adapter>` folder name in the package directory structure. This is a structural guarantee: every check under `proto/` uses the adapter whose `kind() == "proto"` and receives a `ProtoEvolutionContext`. There is exactly one way to determine which adapter a check uses — look at its parent folder.

---

## 7. Rust-native checks (bidirectional support)

Not everything belongs in Starlark. Performance-critical checks, checks requiring complex binary parsing, or checks that need direct access to the Rust async runtime should remain in Rust.

### 7.1 How Rust checks coexist

Rust checks implement the existing `Check` + `ConfiguredCheck` traits. They register directly in `CheckRegistry` with their own ID and metadata. They produce the same `Finding` / `CheckResult` output types as Starlark checks. No Starlark shim file is needed — Rust checks own their identity natively.

Rust checks are activated in two ways:

1. **`CHECKS.yaml`** — a consumer explicitly selects a Rust-native check by ID, the same way it selects Starlark packages.
2. **Always-on** — the runner has a hardcoded set of always-enabled checks (e.g. the `CHECKS.yaml` policy guard). These run regardless of `CHECKS.yaml` configuration.

### 7.2 Rust checks calling Starlark policies

A Rust check can **delegate policy decisions** to user-supplied Starlark, exactly as the proto-evolution branch does today. The Rust side does heavy parsing/diffing; the Starlark side decides what constitutes a violation.

This is the recommended pattern for format adapters: the Rust adapter does parsing, the Starlark check does policy.

### 7.3 Decision framework

| Scenario                                             | Use                             |
| ---------------------------------------------------- | ------------------------------- |
| Policy logic over pre-parsed data                    | Starlark check + Rust adapter   |
| Line/text pattern matching                           | Starlark check + `text` adapter |
| Binary format parsing (protobuf, class files)        | Rust adapter                    |
| Checks needing async I/O or subprocess orchestration | Rust check                      |
| Simple glob + regex rules                            | Starlark check + `text` adapter |

---

## 8. Versioned check distribution

### 8.1 Package identity

Every directory with a `checkleft-package.toml` is a distributable package. The `[package]` table establishes identity.

### 8.2 Resolution

Packages and version sets selected in `CHECKS.yaml` are resolved at `checkleft` startup before any checks run:

1. **`registry://`** — fetched from a check registry (HTTP API). The registry serves tarballs containing `checkleft-package.toml`, published `check.checkleft`/`fix.checkleft` files, and the internal `lib/` files those checks load. Cached locally in `~/.cache/checkleft/packages/<name>/<version>/<sha256>/`.
2. **`git://`** — cloned at the specified tag and packed into the same package byte format. Sparse checkout of the package directory only. Cached similarly and verified against `sha256`.
3. **`path://`** — local filesystem path. For monorepo cross-project dependencies and local tarball iteration. Always relative to the repo root. A `path://tools/my-checks` directory reads live package contents (the directory must contain `checkleft-package.toml`); a `path://dist/acme-checks.tar.gz` archive reads the same publishable tarball format produced by `checkleft_package`. Relative paths (`../`) are not allowed — use the repo-root-relative path instead, similar to Bazel's `//` convention.

### 8.3 Reproducibility and hash pinning

- Only exact versions are supported. No ranges, no `^`, no `~`.
- Fetched packages must declare `sha256` in the `CHECKS.yaml` ref; the resolver verifies fetched bytes before any checks are loaded.
- Version sets are reproducible because the version-set package is itself hash-pinned, and its manifest lists exact constituent package refs and hashes.
- `CHECKS.yaml` package refs and version-set manifests carry the exact versions and hashes that make selected packages reproducible.
- `path://` dependencies are an explicit local-iteration escape hatch. Directory refs read live local content and are not reproducible until replaced by a fetched, hash-pinned ref. Archive refs may supply `sha256`; when present, the resolver verifies the archive bytes before loading package code.
- `checkleft update <dep_name> <new_version>` updates the manifest's exact version and hash.

### 8.4 Publishing

Publishing produces a simple `tar.gz` package. The archive contains `checkleft-package.toml`, published `check.checkleft`/`fix.checkleft` files, and the internal `lib/` files those checks load. It does not vendor package dependencies; consumers activate packages only when they list them directly or select a version set in `CHECKS.yaml`.

The archive layout is rooted at the package itself. The top-level entries are `checkleft-package.toml`, adapter
directories such as `text/` or `proto/`, and optional `lib/` helpers. Consumers
can point `CHECKS.yaml` at the archive with `path://...tar.gz` during local
iteration, or consume the same bytes from `registry://` once published.

The publishable tarball should be buildable by Bazel so check authors can iterate under the same build system that schedules their package tests. See §17 for the `checkleft_package` Bazel rule.

---

## 9. Execution pipeline

### 9.1 Discovery

```
1. Resolve packages and version sets selected by `CHECKS.yaml` (fetch and verify hashes as needed).
2. For each selected package, parse checkleft-package.toml.
3. Auto-discover checks from folder structure in each selected package.
4. Run always-on checks (hardcoded in the runner).
```

### 9.2 Per-check execution

```
1. Determine adapter from check category folder name.
2. Filter changeset by the adapter's file selectors and CHECKS.yaml include/exclude.
3. If no matching files changed, skip (zero-cost).
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
5. If Rust check:
   a. Delegate to Check::configure() + ConfiguredCheck::run() as today.
6. Collect findings.
```

### 9.3 Concurrency

- Starlark checks run on a blocking thread pool (`spawn_blocking`), same as WASM components today.
- Each check gets its own `Module` and `Evaluator` — no shared mutable state.
- Adapter parsing (Rust) can run concurrently across checks.

### 9.4 Error handling

| Error class                        | Behavior                                                               |
| ---------------------------------- | ---------------------------------------------------------------------- |
| `checkleft-package.toml` parse error | Fatal. Package is skipped with error diagnostic.                       |
| `load()` resolution failure        | Fatal for that check. Other checks in the package still run.           |
| Type-check failure in `.checkleft` | Fatal for that check. Reported as a configuration error finding.       |
| Runtime error in `check()`         | Check fails. Finding with `severity: fail` and the Starlark traceback. |
| Adapter parse failure              | Check fails with error. Starlark code is not invoked.                  |

---

## 10. Horizontal scaling, incrementality, and filesystem overhead

The execution model is inherently functional: every `check(ctx)` is a pure function from an immutable context to a list of findings. No shared mutable state, no side effects (hermetic tier), no ordering dependencies between checks. This makes the system trivially horizontally scalable — but only if we manage the filesystem overhead that feeds the pure functions.

### 10.1 Three layers of parallelism

```
┌──────────────────────────────────────────────────────────────────┐
│ Layer 1: Check-level parallelism                                 │
│   Every check gets its own Starlark Module + Evaluator.          │
│   No shared mutable state. Run all checks concurrently.          │
│   Bounded by thread pool size (default: num_cpus).               │
├──────────────────────────────────────────────────────────────────┤
│ Layer 2: Adapter-level parallelism                               │
│   Different adapters (proto, java, module_json) parse            │
│   independent file sets. Run all adapter parses concurrently.    │
│   Within an adapter, base + current parse are independent →      │
│   run both in parallel, then diff.                               │
├──────────────────────────────────────────────────────────────────┤
│ Layer 3: File-level parallelism (within an adapter)              │
│   Adapters that parse files independently (java via tree-sitter, │
│   text) can parse individual files in parallel.                  │
│   Proto is constrained: descriptor generation needs the full     │
│   import graph.                                                  │
└──────────────────────────────────────────────────────────────────┘
```

All three layers compose. In the common case of N checks across M adapters, the execution graph is:

```
1. Discovery (sequential, fast — see §10.4)
2. For each adapter (parallel):
   a. parse(base)  ┐
   b. parse(current)┘  (parallel)
   c. diff(base, current)
3. For each check (parallel, bounded by thread pool):
   a. inject_globals (cheap — borrows parsed output)
   b. evaluate check(ctx) → list[Finding]
```

Step 2 is the bottleneck (generating proto descriptors, running tree-sitter). Step 3 is pure Starlark evaluation — microseconds to low milliseconds per check for typical policy logic.

### 10.2 Adapter output sharing (parse once, check many)

Multiple checks under the same adapter folder share the same parsed output. The runner parses once per `(adapter, file_set, revision)` triple and hands the result to every check under that adapter.

```
proto/evolution/       ─┐
proto/no_deletion/      ├─ all receive the SAME ProtoEvolutionContext
proto/team_policy/    ─┘
```

**Implementation:** The runner groups checks by adapter kind. For each adapter, it calls `parse()` and `diff()` once. The resulting `AdapterOutput` is `Arc`-shared across all checks in that group. `inject_globals()` borrows the shared output — it does not clone it. Starlark values are allocated in each check's own `Module` heap, but they hold references (via `StarlarkValue` wrappers) to the shared Rust-side data.

This means adding a 10th proto check costs approximately zero additional parse time — only the Starlark evaluation time for that check's policy logic.

### 10.3 Why there is no result cache

Caching is unnecessary because the system is already incremental by nature. Checks operate on the **changeset** — the diff between base and current. If a PR touches 3 proto files, the adapter parses only those 3 files (at both revisions), and only checks under adapters whose file selectors match those 3 files run. Everything else is skipped at zero cost (no parse, no Starlark evaluation).

There is no expensive "full repo scan" to cache away. The input is already minimal — it's the diff. Re-running the same check on the same diff is fast enough that caching the result would add complexity (invalidation, storage, staleness) for negligible gain.

### 10.4 Changeset-scoped execution

Before invoking any adapter, the adapter's file selectors are intersected with the changeset. If no changed files match, the adapter and all its checks are skipped entirely — no parse, no Starlark evaluation. Most PRs touch files in one area, so most adapters are irrelevant and skip instantly.

### 10.5 Thread pool and resource bounds

- **Thread pool:** Starlark checks run on a blocking thread pool (`spawn_blocking`). Default size: `num_cpus`. Configurable via `--parallelism=N`.
- **Memory:** Each Starlark `Module` heap is independent. Peak memory is proportional to `(max concurrent checks) × (largest adapter output shared via Arc) + (per-check heap)`. The `Arc`-shared adapter output is the dominant term but is allocated once per adapter, not per check.
- **Starlark evaluation timeout:** Each check has a wall-clock timeout (default: 30s, configurable via `--check-timeout-ms` CLI flag or `CHECKS.yaml` runner settings). Runaway checks are killed and reported as failures. This prevents a single pathological check from blocking the entire pipeline.
- **Adapter parse timeout:** Adapter `parse()` calls have their own timeout (default: 60s). Protoc invocations on large proto graphs are the primary concern here.

---

## 11. Starlark dialect and type system

### 11.1 Dialect settings

```rust
Dialect {
    enable_types: DialectTypes::Enable,
    enable_load: true,
    enable_keyword_only_arguments: true,
    enable_f_strings: true,
    ..Dialect::Standard
}
```

### 11.2 Type annotations

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

### 11.3 Built-in types available in all tiers

```
# Output types
Finding(severity: Severity, message: str, path: str | None, line: int | None, column: int | None, end_line: int | None, end_column: int | None, remediation: str | None, fix_data: struct | None)
FileEdit(path: str, old_text: str, new_text: str, after_line: int | None)
Severity  # enum: fail, fail_but_overridable
Location(path: str, line: int | None, column: int | None, end_line: int | None, end_column: int | None)

# Standard Starlark types
str, int, float, bool, list, dict, None

# Utility
ChangeKind  # enum: added, modified, deleted, renamed
```

### 11.4 Adapter-injected types

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

## 12. Dogfood examples

### 12.1 Proto evolution check

```
checks/
├── checkleft-package.toml
├── lib/
│   └── proto_helpers.checkleft
└── proto/
    └── evolution/
        ├── check.checkleft
        └── fix.checkleft
```

**`checkleft-package.toml`:**

```toml
[package]
name = "mono/checks"
version = "0.1.0"

# Producer metadata only. Consumer activation belongs in CHECKS.yaml.
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

**`proto/evolution/types.checkleft`:**

```python
# Typed fix_data structs shared between check and fix.

FieldNotReserved = struct(
    field_number = int,
    field_name = str,
    insertion_line = int,
)

def field_not_reserved(field_number: int, field_name: str, insertion_line: int) -> FieldNotReserved:
    return FieldNotReserved(
        field_number = field_number,
        field_name = field_name,
        insertion_line = insertion_line,
    )
```

**`proto/evolution/check.checkleft`:**

```python
load("//lib/proto_helpers", "has_reservation", "is_internal_package")
load(":types", "field_not_reserved")

check_meta(
    tier = "hermetic",
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
                fix_data = field_not_reserved(
                    field_number = delta.before_number,
                    field_name = delta.symbol.split(".")[-1],
                    insertion_line = delta.line,
                ),
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
load(":types", "FieldNotReserved")

def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    edits: list[FileEdit] = []
    for f in findings:
        if f.fix_data == None:
            continue
        if type(f.fix_data) == FieldNotReserved:
            edits.append(file_edit(
                path = f.path,
                old_text = "",
                new_text = "  reserved {};\n  reserved \"{}\";\n".format(
                    f.fix_data.field_number,
                    f.fix_data.field_name,
                ),
                after_line = f.fix_data.insertion_line,
            ))
    return edits
```

### 12.2 `module.json` required-fields check

```
module-checks/
├── checkleft-package.toml
└── module_json/
    └── required_fields/
        └── check.checkleft
```

**`module_json/required_fields/check.checkleft`:**

```python
check_meta(
    tier = "hermetic",
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

### 12.3 Java API stability check

```
java-checks/
├── checkleft-package.toml
└── java/
    └── api_stability/
        └── check.checkleft
```

**`java/api_stability/check.checkleft`:**

```python
check_meta(
    tier = "hermetic",
)

TRACKED_VISIBILITY: list[str] = ["public", "protected"]

def check(ctx: JavaEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

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

## 13. Functional testing for checks

Check authors need to test their checks by running them against real file inputs and asserting on the outputs. Tests use **file fixtures** — actual files on disk organized into `before/` and `after/` workspaces. The test runner diffs the two workspaces, runs the adapter + check, and compares findings against expected output.

This is a purely functional, black-box approach: files in, findings out. No mocking, no stubbing the adapter.

### 13.1 Test directory structure

Each test case is a directory under `testdata/` containing a `before/` workspace, an `after/` workspace, and an `expected.toml` file declaring the expected findings. The test path is part of the authoring API: check IDs come from `<adapter>/<name>`, and test cases live directly beside the check they exercise.

```
proto/evolution/
├── check.checkleft
├── fix.checkleft
└── testdata/
    ├── field_removal/                     # test case name
    │   ├── before/
    │   │   └── api/v1/user.proto          # base revision files
    │   ├── after/
    │   │   └── api/v1/user.proto          # current revision files
    │   └── expected.toml                  # expected findings
    ├── field_addition/
    │   ├── before/
    │   │   └── api/v1/user.proto
    │   ├── after/
    │   │   └── api/v1/user.proto
    │   └── expected.toml
    └── file_deletion/
        ├── before/
        │   └── api/v1/user.proto
        ├── after/                         # empty — file was deleted
        └── expected.toml
```

### 13.2 Fixture files

**`before/`** — contains the files as they existed at the base revision. The adapter parses these as the "before" state.

**`after/`** — contains the files as they exist at the current revision. The adapter parses these as the "after" state. Missing files (present in `before/` but absent in `after/`) are treated as deletions. New files (present in `after/` but absent in `before/`) are treated as additions.

The test runner computes the diff between the two workspaces, runs the adapter to parse both sides, and invokes the check — exactly as a real checkleft run would.

### 13.3 Expected output (`expected.toml`)

```toml
# testdata/field_removal/expected.toml

[[findings]]
severity = "fail"
message_contains = "must be reserved"    # substring match on the message
path = "api/v1/user.proto"

# For a test that should produce zero findings:
# testdata/field_addition/expected.toml
# (empty file or explicit empty list)
# findings = []
```

**`expected.toml` fields:**

| Field                         | Type   | Required | Description                                                 |
| ----------------------------- | ------ | -------- | ----------------------------------------------------------- |
| `findings`                    | `list` | no       | Expected findings. Empty or omitted = expect zero findings. |
| `findings[].severity`         | `str`  | yes      | `"fail"` or `"fail_but_overridable"`.                       |
| `findings[].message_contains` | `str`  | no       | Substring that must appear in the finding message.          |
| `findings[].message_eq`       | `str`  | no       | Exact message match (alternative to `message_contains`).    |
| `findings[].path`             | `str`  | yes      | File path the finding should be on.                         |
| `findings[].line`             | `int`  | no       | Expected line number.                                       |

### 13.4 Fix testing

If a check has a `fix.checkleft`, test cases can include an `expected_fix/` directory alongside `expected.toml`. After running the check, the test runner runs the fix and asserts that the resulting files match `expected_fix/`.

```
testdata/field_removal/
├── before/
│   └── api/v1/user.proto
├── after/
│   └── api/v1/user.proto           # field removed, no reservation
├── expected.toml                    # expects a "must be reserved" finding
└── expected_fix/
    └── api/v1/user.proto           # after fix: reservation added
```

The test runner applies the fix edits to the `after/` workspace and diffs the result against `expected_fix/`. Any mismatch fails the test with a unified diff.

### 13.5 Running tests

```bash
# Run all tests for all checks in the package
checkleft test

# Run all tests for a specific check
checkleft test proto/evolution

# Run a specific test case
checkleft test proto/evolution/field_removal

# Update expected output from actual results (snapshot testing)
checkleft test --update proto/evolution
```

Check authors can schedule the same test flow in Bazel with `checkleft_test`. The BUILD file lives next to the check itself — one target per check:

```
proto/evolution/
├── BUILD
├── check.checkleft
├── fix.checkleft
├── types.checkleft
└── testdata/
    └── field_removal/
        ├── before/
        ├── after/
        └── expected.toml
```

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft_test")

checkleft_test(
    name = "proto_evolution_test",
    srcs = glob(["*.checkleft"]),
    testdata = glob(["testdata/**"]),
    deps = ["//checks/lib:proto_helpers"],
)
```

| Attribute  | Type            | Required | Description                                                                                       |
| ---------- | --------------- | -------- | ------------------------------------------------------------------------------------------------- |
| `srcs`     | `list[label]`   | yes      | All `.checkleft` files for this check (check, fix, local helpers). Must contain exactly one `check.checkleft`. |
| `testdata` | `list[label]`   | yes      | Fixture files: `testdata/<case>/{before/, after/, expected.toml, expected_fix/}`.                 |
| `deps`     | `list[label]`   | no       | Package-level `lib/` helpers or filegroups that this check `load()`s.                             |

The rule validates that exactly one `check.checkleft` exists in `srcs`. `fix.checkleft` presence is detected automatically. Each target is independently cacheable and parallelizable.

The checkleft binary is a private attribute defaulting to `//tools/checkleft:checkleft` — the compiled-from-source binary target.

### 13.6 Testing network-tier checks

Network-tier checks use `http_get()`, which the test runner mocks at the Rust level. The test author declares HTTP responses in `expected.toml` — the runner injects a mock `http_get` that replays them instead of making real network calls.

```toml
# testdata/field_not_reserved_remotely/expected.toml

# Mock HTTP responses — matched in order per URL.
# Multiple entries for the same URL simulate retries/failures.
[[http_mocks]]
url = "https://reservations.internal.acme.com/api/reserved/User.old_field/3"
status = 404
body = ""

[[http_mocks]]
url = "https://reservations.internal.acme.com/api/reserved/User.active_field/1"
status = 200
body = '{"reserved": true}'

# Simulate a transient failure followed by success (for retry logic)
[[http_mocks]]
url = "https://reservations.internal.acme.com/api/reserved/User.flaky_field/5"
status = 503
body = "service unavailable"

[[http_mocks]]
url = "https://reservations.internal.acme.com/api/reserved/User.flaky_field/5"
status = 200
body = '{"reserved": true}'

[[findings]]
severity = "fail"
message_contains = "not reserved in the central reservation service"
path = "api/v1/user.proto"
```

The mock is URL-matched and consumed in declaration order — the first call to a URL returns the first matching entry, the second call returns the next, etc. This lets test authors express retries, transient failures, and varying responses without any code.

The test runner replaces the real `http_get` in the Starlark globals with the mock. The check code is unaware — it calls `http_get()` the same way it would in production.

### 13.7 Test discovery and execution

- Any directory under `testdata/` that contains both a `before/` (or empty) and an `expected.toml` is a test case.
- Tests run hermetically — the adapter + check execute against the fixture files, with no access to the real repo.
- Each test case runs independently. No shared state between test cases.
- Test failures show: expected findings vs. actual findings, with diffs.
- `--update` flag regenerates `expected.toml` from actual output (snapshot update).

---

## 14. Integration with existing checkleft infrastructure

### 14.1 Output compatibility

Starlark checks produce `Finding` values that map to the existing checkleft diagnostic output:

| Starlark `Finding` field   | Rust `Finding` field                                              |
| -------------------------- | ----------------------------------------------------------------- |
| `severity`                 | `severity`                                                        |
| `message`                  | `message`                                                         |
| `path` + optional line/column/span fields | `location: Option<Location>`                         |
| `remediation`              | `remediation: Option<String>`                                     |
| `fix_data`                 | `fix_data: Option<StarlarkValue>` (opaque, passed through to fix) |

`message` is the primary human-facing diagnostic text and may include remediation instructions directly. `remediation` is optional secondary human-facing text for checks that want a separate suggested-action field. Both are displayed inline with the file, line, point, or span location supplied by the check. Neither implies an automatic edit.

### 14.2 GitHub PR annotations

Checkleft findings must be exportable to GitHub PR annotations. This lets checkleft errors appear inline in the pull request file view and checks UI.

The GitHub annotation shape is:

```json
{
  "path": "api/v1/user.proto",
  "start_line": 17,
  "end_line": 17,
  "start_column": 3,
  "end_column": 19,
  "annotation_level": "failure",
  "title": "proto/evolution",
  "message": "removed field number must be reserved"
}
```

Checkleft machine-readable output must include this normalized shape when it can be derived from the finding. The native finding remains the source of truth; the GitHub object is a renderer-ready projection.

```json
{
  "check_id": "proto/evolution",
  "severity": "fail_but_overridable",
  "overridable": true,
  "message": "removed field number must be reserved",
  "location": {
    "path": "api/v1/user.proto",
    "line": 17,
    "column": 3,
    "end_line": 17,
    "end_column": 19
  },
  "github_annotation": {
    "path": "api/v1/user.proto",
    "start_line": 17,
    "end_line": 17,
    "start_column": 3,
    "end_column": 19,
    "annotation_level": "failure",
    "title": "proto/evolution",
    "message": "removed field number must be reserved"
  }
}
```

`github_annotation` is present when `path` and `line` are present. It is omitted for file-only findings unless the renderer explicitly chooses to anchor file-only diagnostics to line 1. This keeps the default JSON honest: a missing line cannot masquerade as a precise line annotation.

Mapping:

| Checkleft field             | GitHub annotation field                                                   |
| --------------------------- | ------------------------------------------------------------------------- |
| `path`                      | `path`                                                                    |
| `line`                      | `start_line`                                                              |
| `end_line` or `line`        | `end_line`                                                                |
| `column`                    | `start_column` when `start_line == end_line`                              |
| `end_column`                | `end_column` when `start_line == end_line`                                |
| `Severity.fail`             | `annotation_level = "failure"`                                            |
| `Severity.fail_but_overridable` | `annotation_level = "failure"`                                        |
| check ID                    | `title`                                                                   |
| `message`                   | `message`                                                                 |
| `remediation`               | appended to `message` or emitted as `raw_details`, depending on renderer |

GitHub annotations require line numbers. File-only findings still remain valid checkleft findings, but a GitHub renderer must anchor them to line 1 or put them in the check-run summary instead of creating a line annotation. Multi-line spans omit column fields because GitHub only supports column ranges for same-line annotations.

Both Checkleft severities map to GitHub `failure` annotations because both severities are blocking findings. Overridability is Checkleft policy metadata (`severity = "fail_but_overridable"` / `overridable = true`), not a GitHub warning level.

When checkleft runs inside GitHub Actions without a GitHub App integration, the runner can also emit workflow commands such as `::error file=...,line=...,col=...::message`. This produces GitHub annotations from stdout. The richer Checks API path is preferred for first-class check-run output because it supports appendable annotations and summary text.

### 14.3 Fix compatibility

Starlark `fix()` functions return `list[FileEdit]` which maps to the existing `Vec<FileEdit>` consumed by `WritableSandbox`. The existing fix scheduler (`src/fix/scheduler.rs`) orchestrates Starlark fixes identically to WASM component fixes.

### 14.4 Progress reporting

The runner reports Starlark check progress through the existing `ProgressReporter` trait. Each adapter registers its `applicable_file_count` (derived from file selector matching against the changeset) and ticks progress as files are processed.

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

# Finding with a precise source span
findings.append(finding(
    severity = Severity.fail,
    message = "removed field number must be reserved",
    path = "api/v1/user.proto",
    line = 17,
    column = 3,
    end_line = 17,
    end_column = 19,
    remediation = "Add a reserved statement for field number 4.",
))

# Finding with remediation included directly in the message
findings.append(finding(
    severity = Severity.fail,
    message = "required key 'version' removed from module.json; restore it because it is required by the module loader",
    path = "services/auth/module.json",
))

# Finding with separate remediation guidance
findings.append(finding(
    severity = Severity.fail,
    message = "required key 'version' removed from module.json",
    path = "services/auth/module.json",
    remediation = "Restore the 'version' key. It is required by the module loader.",
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
load(":types", "FieldNotReserved", "field_not_reserved")

# Multiple symbols from one module
load("//lib/matchers", "glob_match", "path_prefix", "is_generated_file")
```

### 15.4 Defining shared helper modules

**`lib/proto_helpers.checkleft`:**

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

The fix function receives typed `fix_data` from the check (see §4.2). No string parsing — pattern-match on the struct type.

```python
# proto/evolution/fix.checkleft

load(":types", "FieldNotReserved")

def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    """Generate file edits to auto-fix findings where possible."""
    edits: list[FileEdit] = []

    for f in findings:
        if f.fix_data == None:
            continue

        if type(f.fix_data) == FieldNotReserved:
            edits.append(file_edit(
                path = f.path,
                old_text = "",
                new_text = "  reserved {};\n  reserved \"{}\";\n".format(
                    f.fix_data.field_number,
                    f.fix_data.field_name,
                ),
                after_line = f.fix_data.insertion_line,
            ))

    return edits
```

### 15.12 `CHECKS.yaml` selecting a version set

```yaml
# CHECKS.yaml
# Instead of pinning 5+ individual check packages, depend on one version set.

checkleft_packages:
  version_sets:
    - source: registry://checkleft-hub/acme-versionset
      version: "2025.06.1"
      sha256: "b3d1000000000000000000000000000000000000000000000000000000000000"

  packages:
    - source: git://github.com/myteam/checkleft-checks.git
      version: "0.3.0"
      sha256: "9f200000000000000000000000000000000000000000000000000000000000"
      mode: all

checks:
  - id: proto/evolution
    include:
      - "api/**/*.proto"
```

### 15.13 Network tier: checking field reservations against a remote service

```python
# checkleft/proto/reservation_check/check.checkleft

check_meta(
    tier = "network",
)

RESERVATION_SERVICE_URL: str = "https://reservations.internal.acme.com"

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    """Verify that removed fields are reserved in the central reservation service."""
    findings: list[Finding] = []
    base_url: str = RESERVATION_SERVICE_URL

    for delta in ctx.deltas:
        if delta.kind != DeltaKind.field_removed:
            continue

        # Ask the reservation service if this field number is reserved
        resp: HttpResponse = http_get(
            "{}/api/reserved/{}/{}".format(base_url, delta.symbol, delta.before_number),
            timeout_ms = 5000,
        )

        if resp.status == 200:
            # Field is registered as reserved — good
            continue

        if resp.status == 404:
            # Not reserved in the central service
            findings.append(fail(
                message = "removed field {} (number {}) is not reserved in the central reservation service".format(
                    delta.symbol, delta.before_number,
                ),
                path = delta.path,
                remediation = "Register the field reservation at {}/reserve before removing it.".format(base_url),
            ))
        else:
            findings.append(fail(
                message = "failed to check reservation status for {}: HTTP {}".format(
                    delta.symbol, resp.status,
                ),
                path = delta.path,
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

---

## 16. Summary of conventions

| Convention          | Rule                                                                                                 |
| ------------------- | ---------------------------------------------------------------------------------------------------- |
| File extension      | `.checkleft` always                                                                                  |
| Check location      | `<package_root>/<adapter>/<name>/check.checkleft`                                                    |
| Fix location        | `<package_root>/<adapter>/<name>/fix.checkleft`                                                      |
| Shared code         | `<package_root>/lib/*.checkleft`                                                                     |
| Check-local helpers | `<package_root>/<adapter>/<name>/<anything>.checkleft` (not `check`, `fix`, or `check_test`)         |
| Package manifest    | `checkleft-package.toml` (presence marks a directory as a package root)                              |
| Package integrity   | Exact `source`/`version`/`sha256` refs in `CHECKS.yaml` and version-set includes                    |
| Check ID            | `<adapter>/<name>` (e.g. `proto/evolution`)                                                          |
| Type annotations    | Required on all function signatures                                                                  |
| Default sandbox     | `hermetic`                                                                                           |
| Adapter linkage     | `<adapter>` top-level folder name matches `FormatAdapter::kind()`                                    |
| Fix data invariant  | Auto-fixable findings must carry typed `fix_data`; `fix.checkleft` is the only programmatic fix path |

---

## 17. Bazel rules

All Bazel rules use the compiled-from-source checkleft binary (`//tools/checkleft:checkleft`) as a private attribute. No toolchain abstraction — the binary target is referenced directly.

### 17.1 `checkleft_test` — fixture testing

Runs `checkleft test` for a single check against its fixture test cases. One target per check, BUILD file lives next to the check.

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft_test")

checkleft_test(
    name = "proto_evolution_test",
    srcs = glob(["*.checkleft"]),
    testdata = glob(["testdata/**"]),
    deps = ["//checks/lib:proto_helpers"],
)
```

| Attribute  | Type            | Required | Description                                                                                       |
| ---------- | --------------- | -------- | ------------------------------------------------------------------------------------------------- |
| `srcs`     | `list[label]`   | yes      | All `.checkleft` files for this check. Must contain exactly one `check.checkleft`.                |
| `testdata` | `list[label]`   | yes      | Fixture files: `testdata/<case>/{before/, after/, expected.toml, expected_fix/}`.                 |
| `deps`     | `list[label]`   | no       | Package-level `lib/` helpers or filegroups.                                                       |

Validates that exactly one `check.checkleft` exists in `srcs`. Detects `fix.checkleft` automatically. Runs validation (type-checking, `check_meta()` presence, load path resolution, `fix_data` contract) as an implicit first step before executing fixtures. Each target is independently cacheable and parallelizable.

### 17.2 `checkleft_validate` — type-checking without fixtures

Validates a single check without running fixtures. Useful for fast CI feedback before test cases exist, or for checks that have no `testdata/` yet.

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft_validate")

checkleft_validate(
    name = "proto_naming_validate",
    srcs = glob(["*.checkleft"]),
    deps = ["//checks/lib:proto_helpers"],
)
```

| Attribute  | Type            | Required | Description                                                         |
| ---------- | --------------- | -------- | ------------------------------------------------------------------- |
| `srcs`     | `list[label]`   | yes      | All `.checkleft` files for this check. Must contain one `check.checkleft`. |
| `deps`     | `list[label]`   | no       | Package-level `lib/` helpers or filegroups.                         |

Runs the same validation as `checkleft_test` (type-checking, `check_meta()` presence, adapter folder name, load paths, `fix_data` contract) but does not require or execute fixtures.

### 17.3 `checkleft_package` — publishable tarball

Builds a deterministic `.tar.gz` archive for a check package. The BUILD file lives at the package root.

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft_package")

checkleft_package(
    name = "my_checks_pkg",
    manifest = "checkleft-package.toml",
    checks = [
        "//checks/proto/evolution:proto_evolution_test",
        "//checks/proto/naming:proto_naming_validate",
        "//checks/text/no_debug:no_debug_test",
    ],
    lib = glob(["lib/*.checkleft"]),
)
```

| Attribute  | Type            | Required | Description                                                                                       |
| ---------- | --------------- | -------- | ------------------------------------------------------------------------------------------------- |
| `manifest` | `label`         | yes      | The `checkleft-package.toml` file.                                                                |
| `checks`   | `list[label]`   | yes      | `checkleft_test` or `checkleft_validate` targets. Each check's `srcs` are included in the archive. |
| `lib`      | `list[label]`   | no       | Package-level `lib/*.checkleft` helper files included in the archive.                             |

The archive layout is rooted at the package: `checkleft-package.toml`, adapter directories, and `lib/`. `testdata/` is excluded. Validation is implicit — all referenced check targets must pass before the archive is emitted.

### 17.4 Default-enabled checks

The runner has a hardcoded set of always-on checks that run regardless of `CHECKS.yaml`. These include both Rust-native checks and Starlark checks (e.g. the `CHECKS.yaml` policy guard).

Starlark checks that are always-on are embedded into the checkleft binary at compile time. The `rust_binary` target for checkleft depends on `checkleft_package` targets that produce the default check archives. At build time, these archives are included via `include_bytes!` and unpacked at runtime — one binary, no external files.

```starlark
# tools/checkleft/BUILD

rust_binary(
    name = "checkleft",
    srcs = glob(["src/**/*.rs"]),
    data = [
        "//tools/checkleft/default-checks:policy_guard_pkg",
    ],
    # ...
)
```

The runner loads default check packages from its embedded data before processing `CHECKS.yaml`-selected packages. Default checks cannot be disabled by consumers.
