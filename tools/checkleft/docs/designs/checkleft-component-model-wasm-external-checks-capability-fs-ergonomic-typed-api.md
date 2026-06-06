# Checkleft: Component-Model wasm external checks (capability FS + ergonomic typed API)

Status: design (no implementation in this change)
Project: P1446 / `proj_18b65280ce1e7948_2bd`

## Overview

Checkleft runs three kinds of checks: built-in (compiled into the binary), declarative (`declarative-v1`: framework-owned binary invocations + declarative transforms), and wasm-external (`sandbox-v1`: a sandboxed wasm artifact). Most checks will live **out-of-tree** — independently versioned, authored by teams that do not ship inside the checkleft binary — and we want sandboxed, any-language-ish authoring for them.

The current `sandbox-v1` wasm runtime is a hand-rolled CORE-wasm ABI. Authors write raw pointer/length code, manage linear-memory buffers by hand, cannot read any files, and run under a fixed, tight fuel budget. None of that pain is intrinsic to wasm — it is intrinsic to a bespoke ABI that reimplements, badly, what the **WebAssembly Component Model** already standardizes.

This document audits `sandbox-v1`, then proposes re-architecting the wasm tier onto the Component Model so that:

1. **Ergonomic typed authoring** — an author writes `fn check(input: CheckInput) -> Vec<Finding>` against real Rust structs. No pointers, no manual `memory.grow`/offset math. All marshalling is generated glue (`wit-bindgen` on the guest, `wasmtime::component::bindgen!` on the host).
2. **Capability-scoped file access** — checks *can* read files, but the **host** decides and enforces exactly which paths each invocation may read. Deny-by-default, finer than whole-FS.

The current `sandbox-v1` restrictions are not sacrosanct; a fresh-slate rewrite of the wasm tier is explicitly in scope and is what this design recommends.

## Goals

- Out-of-tree checks are authored as plain typed Rust functions over generated native structs — no raw pointers, no manual linear-memory management. The guest SDK hides the ABI entirely.
- The host grants each check invocation a capability-scoped, deny-by-default set of readable files, enforced host-side and finer than whole-filesystem.
- One artifact can export many checks (one component, N checks), self-describing enough that the bundled provider + CHECKS source directive can enumerate them.
- Timeouts and memory are policy knobs with generous defaults and per-check / per-bundle overrides — not the prototype's fixed, tight fuel ceiling.
- Checks run in-process (no per-invocation process spawn), AOT-compiled and cached, with a quantified cost story versus the current ABI and versus the exec/declarative alternative.
- The existing executor/provider/runner architecture, sha256 artifact pinning, and CHECKS resolution semantics are preserved and reworked, not thrown away.
- The end-to-end proof is porting `rust-giant-structs-use-builder` (currently a built-in; ported to `sandbox-v1` in T1444/PR 1410) onto the new model.
- **Bazel is the required build path — for both the runtime and every check.** "You can build it with cargo" and "there is a shell script to build the check" are not acceptable endpoints. The bazel build is the real, supported path. Both the wasmtime host embedding and the out-of-tree guest components must build under bazel, including wit-bindgen guest-binding generation, wasm32-wasip2 cross-compilation, and the componentization step. Building custom bazel rules to support this is explicitly acceptable and expected (see §Build (bazel)).

## Non-goals

- Multi-language guest authoring in v1. Rust-only is acceptable and aligns with the wit-bindgen/`cargo component` sweet spot (this preserves the T1371/PR 1372 decision). The WIT contract does not preclude other guest languages later, but no non-Rust SDK ships in v1.
- Replacing or re-litigating the **declarative** (`declarative-v1`) tier. The declarative tier keeps owning the "run an arbitrary binary" use case (e.g. `buildifier`). This project only re-architects the **wasm** tier.
- Network access from checks. Checks remain pure-compute plus host-mediated file reads. No host-imported network capability.
- Write access to the working tree from checks. `SuggestedFix`/`FileEdit` data flows *out* as part of findings (already in the output schema); checks never mutate files directly.
- A general OS-process sandbox for wasm guests. Wasm's own isolation plus the host capability boundary is the sandbox; we are not adding seccomp/Landlock/sandbox-exec around the host process for this tier.
- Remote/registry distribution of components beyond what the existing external-URL provider path already does. Fetch/caching of remote components is called out as future work, not a v1 blocker.

## Audit of the current `sandbox-v1` runtime

Source: `tools/checkleft/src/external/runtime.rs`, `mod.rs`, `command_policy.rs`, `bundled.rs`, `provider.rs`, and `runtime/tests.rs`.

### How it works today

- **Two execution paths in one function.** `DefaultExternalCheckExecutor::execute_artifact` first tries a CORE-wasm module path (`execute_core_artifact`); on an `ArtifactMismatch` it falls back to a "component" path (`execute_component_artifact`).
- **The core path is a hand-rolled ABI.** The module must export `memory` and `checkleft_run: (i32, i32) -> i64`. The host serializes `{changeset, config, capabilities}` to JSON, writes it into the guest's linear memory at offset 0 (growing memory by hand via `ensure_memory_capacity`), calls `checkleft_run(offset, len)`, and decodes the `i64` return as a packed `(offset << 32) | len` pointing at the output JSON, which it then reads back out of linear memory. The guest is responsible for allocating that output buffer and packing the pointer.
- **The "component" path is not a real Component-Model interface.** It expects an export `run: (string) -> (string)` and passes the *same JSON string* both ways (`call_component_run`). It uses `wasmtime::component`, but there are **no WIT records** — no lifting/lowering of typed structs. It is the core ABI's manual JSON marshalling wearing a component costume.
- **Limits.** Fuel only, fixed at `EXECUTION_FUEL_LIMIT = 10_000_000`, set per store. No epoch deadline, no wall-clock timeout, no configured memory cap, no per-check override.
- **No AOT / no cache.** `Module::new` / `Component::new` recompile the artifact from bytes on **every** invocation. The `Engine` is reused, but compiled artifacts are not cached to disk (`.cwasm`) or in memory across runs.
- **Capabilities are vestigial for wasm.** The manifest `capabilities.commands` list is intersected with a global ceiling (`cat`, `grep`, `sed`, `wc`) in `command_policy.rs` and serialized into the input JSON — but the guest is pure wasm with **no host imports**, so there is no way for a guest to actually run a command. The grant is plumbed but inert in this tier. (It is a leftover from when wasm and exec shared a path; the exec use case now lives in the declarative tier.)
- **sha256 pinning works.** `validate_artifact_sha256` rejects any artifact whose bytes don't match the manifest digest. This is good and must survive.
- **One artifact = one check.** The manifest (`mode = wasm`, `runtime = sandbox-v1`, `artifact_path`, `artifact_sha256`) describes a single check. There is no bundle / `list-checks` concept.

### Gap list against the two hard requirements

| Requirement | Current state | Gap |
|---|---|---|
| Ergonomic typed authoring | Guest hand-writes `checkleft_run(i32,i32)->i64`, allocates output buffer, packs pointer; or `run(string)->(string)` with manual JSON. | No typed records, no generated glue. Authors see pointers and linear memory. Fails the requirement. |
| Capability-scoped file access | `execute()` receives `&dyn SourceTree` but ignores it (`_source_tree`). Guest gets `{changeset, config, capabilities}` JSON only. No host file import. | Checks cannot read files at all. This is almost certainly because `sandbox-v1` is bare core-wasm with **no WASI wired in** — there are no host imports for file I/O and no WASI preopen. The fix is to **adopt WASI** (specifically `wasm32-wasip2`), not to design a bespoke file-access ABI. Fails the requirement. |
| Limits as policy | Fixed `consume_fuel` budget, no overrides, no timeout, no memory cap. | No epoch deadline, no configurable memory, no per-check/per-bundle knobs. |
| Performance | Recompiles artifact every invocation; no `.cwasm` cache. | Wasted compile cost per run; the one thing wasm-in-process is supposed to win (cheap repeated invocation) is left on the table. |
| Bundling/discovery | One artifact, one check. | No "one component, N checks" / `list-checks`. |
| Capability surface clarity | `capabilities.commands` plumbed into wasm input but unusable by a pure guest. | Misleading dead surface that should be removed from the wasm tier. |

**Conclusion of the audit:** the prototype's pain is the bespoke ABI, not wasm. The Component Model directly closes the two top-row gaps (typed authoring, capability file access via host imports) and gives us the limit/cache/bundling story for the rest. The bespoke path should be superseded, not evolved.

## Alternatives considered

### A. Evolve the bespoke core-wasm ABI

Add host-imported file-read functions to the existing `checkleft_run` core ABI and write a hand-rolled codegen layer that marshals typed structs across the `i32`/`i64` buffer boundary.

Rejected: this reinvents, by hand and worse, exactly what the Component Model's Canonical ABI already standardizes (lifting/lowering of records, strings, lists, results, options). Every new field on `CheckInput`/`Finding` becomes manual marshalling work on both sides. There is no typed contract artifact, so guest and host can silently disagree on layout. We would own and debug a marshalling layer forever to avoid adopting the one the ecosystem already maintains.

### B. exec-external (binary + JSON-over-stdio, OS-sandboxed)

We came close to choosing this route project-wide. A check is an arbitrary binary; the host invokes it with a JSON payload on stdin, reads findings on stdout, and wraps it in an OS sandbox (bazel-style).

Rejected **as the wasm-tier replacement**, but explicitly **kept for what it is good at.** The exec model *already exists* as the declarative tier (`declarative-v1`), which subsumes the former exec tier via the `passthrough` transform and owns the "wrap an existing binary like buildifier" use case. For the *typed-Rust-check* sweet spot it loses on three axes: (1) per-invocation process spawn cost (~1-5 ms each) versus in-process instantiation (~tens of µs); (2) OS-sandbox portability — a deny-by-default file capability that behaves identically on macOS and Linux CI agents is genuinely hard with `sandbox-exec`/Landlock/seccomp and is precisely the problem the declarative tier defers; (3) "typed function over real structs" still requires every author to hand-roll JSON (de)serialization in their language. The two tiers are complementary: declarative for "any binary, framework-invoked," wasm/component for "sandboxed typed check with host-scoped file reads."

### C. Native dynamic plugins (`cdylib` + `dlopen`)

Compile checks to native shared libraries and load them with a stable C ABI.

Rejected: zero sandboxing (a plugin runs with full host privileges, defeating requirement #2's entire premise), fragile ABI across compiler/std versions, and `unsafe` loading of untrusted out-of-tree code into the checkleft process. Non-starter for out-of-tree authoring.

### D. WebAssembly Component Model — **chosen**

WIT-defined contract, `wit-bindgen` guest SDK, `wasmtime::component::bindgen!` host, host-imported capability-scoped file access, epoch/memory limits, `.cwasm` cache. Detailed below.

## Chosen approach: the Component Model

### Architecture at a glance

```
author writes:                 fn check(input: CheckInput) -> Vec<Finding>
                                      |  (wit-bindgen generates the glue)
guest crate (checkleft-check-sdk) --> wasm32-wasip2 component (exports checkleft:check world)
                                      |  cargo component build -> .wasm; bazel rule emits sha256
manifest (mode=component) pins artifact_path + artifact_sha256 + access-scope + limits
                                      |
host (checkleft) -- component::bindgen! --> instantiate (cached .cwasm)
   - invokes shared FS-sandbox module: resolves allowlist from declared access scope,
     materialises files into per-invocation temp dir (preserving repo-relative paths)
   - preopens sandbox root as WASI root (guest reads via std::fs, no special API)
   - epoch deadline + memory ResourceLimiter
   - calls list-checks() / run-check(name, input) -> list<finding>
```

### WIT contract

A new in-tree WIT package, e.g. `wit/check.wit`, defines `package checkleft:check@0.1.0`. The records mirror today's `input.rs` / `output.rs` types so the host lift/lower is mechanical. Sketch (illustrative, not final):

```wit
package checkleft:check@0.1.0;

interface types {
  enum change-kind { added, modified, deleted, renamed }
  record changed-file { path: string, kind: change-kind, old-path: option<string> }
  record file-line-delta { added-lines: u32, removed-lines: u32 }
  record diff-hunk {
    old-start: u32, old-lines: u32, new-start: u32, new-lines: u32,
    added-lines: u32, removed-lines: u32,
  }
  record file-diff { path: string, hunks: list<diff-hunk> }

  record change-set {
    changed-files: list<changed-file>,
    file-diffs: list<file-diff>,
    commit-description: option<string>,
    pr-description: option<string>,
    change-id: option<string>,
    repository: option<string>,
  }

  // Per-check config is dynamic (toml::Value today). v1 passes it as a JSON
  // string the guest SDK deserializes with serde into the author's own config struct.
  record check-input { changeset: change-set, config-json: string }

  enum severity { error, warning, info }
  record location { path: string, line: option<u32>, column: option<u32> }
  record file-edit { path: string, old-text: string, new-text: string }
  record suggested-fix { description: string, edits: list<file-edit> }
  record finding {
    severity: severity,
    message: string,
    location: option<location>,
    remediations: list<string>,
    suggested-fix: option<suggested-fix>,
  }

  // How much of the repository a check declares it needs to read.
  // Absent means modified-only (the default; safe for most checks).
  variant access-scope {
    // Only the files modified in the current changeset (default when absent).
    modified-only,
    // Every file in the repository tree.  Opt-in; host may apply extra scrutiny.
    whole-repo,
    // Union of the declared globs (repo-root-relative) plus all changeset files.
    // The host intersects the glob expansion with a ceiling.
    globs(list<string>),
  }

  // Self-description for bundling/discovery.
  record check-descriptor {
    name: string,
    description: string,
    default-severity: severity,
    // Declared file-access scope.  Absent means modified-only (the default).
    access-scope: option<access-scope>,
  }

  variant check-error { unknown-check(string), failed(string) }
}

world check {
  use types.{check-input, finding, check-descriptor, check-error};
  // File access is via std::fs (WASI-p2), not a host-imported function.
  // The host preopens a sandbox dir containing the allowlisted files;
  // guests read normally with no checkleft-specific API.
  list-checks: func() -> list<check-descriptor>;
  run-check: func(name: string, input: check-input) -> result<list<finding>, check-error>;
}
```

Design choices baked into the contract:

- **`list-checks` + `run-check(name, input)`** make one component self-describing and able to export N checks (the "one artifact, N checks" requirement). The host instantiates once, calls `list-checks()` to enumerate, and dispatches by name.
- **File access is via `std::fs`, not a WIT import.** The `wasm32-wasip2` target maps `std::fs` onto WASI-p2 interfaces that wasmtime services; the host enforces capability scoping by what it places in the preopen sandbox dir (see file-capability section). No checkleft-specific file API exists in the WIT world.
- **`access-scope` declares the check's file-access appetite.** The default (absent or `modified-only`) gives a check only the files touched by the changeset — correct and safe for most checks. `whole-repo` is an explicit opt-in for checks that must traverse the full tree. `globs(patterns)` is for targeted cross-file reads (e.g. config files alongside source); modified files are always included. The shared FS-sandbox module (see §Shared FS sandbox module) resolves the declared scope into the materialised sandbox that the wasm runtime preopens as the WASI root.
- **Config as `config-json`.** `toml::Value` is dynamic; modeling arbitrary config as WIT records would force a schema per check or a recursive `variant`. v1 passes config as a JSON string and the guest SDK deserializes it into the author's own `#[derive(Deserialize)]` config struct. This is the recommended pragmatic choice; a typed-per-check generic is a possible future evolution but is not a v1 concern.

### Guest SDK + build pipeline

- A new crate `checkleft-check-sdk` (name TBD) wraps `wit-bindgen` so authors implement a small trait and register checks. Target ergonomics:

```rust
use checkleft_check_sdk::{check, CheckInput, Finding, Severity};

// No access_scope → modified-only by default: sandbox contains only changed files.
#[check(name = "rust-giant-structs-use-builder")]
fn run(input: CheckInput) -> Vec<Finding> {
    let cfg: MyConfig = input.config()?;              // serde over config-json
    let src = std::fs::read_to_string(&path)?;        // normal Rust; sandbox dir enforces access
    // ... pure analysis ...
    vec![Finding::error("…").at(path, line)]
}

// Example: a check that also needs Cargo.toml files alongside changed sources.
#[check(name = "dep-policy", access_scope = globs(["**/Cargo.toml"]))]
fn check_deps(input: CheckInput) -> Vec<Finding> { /* ... */ }
```

  The `#[check]` macro registers the function in the component's `list-checks`/`run-check` dispatch table and records the declared `access-scope` (defaulting to `modified-only`). Authors never touch the WIT bindings, pointers, or memory.

- **Build:** `cargo component build --target wasm32-wasip2` produces the component `.wasm`. The `wasm32-wasip2` target is required (not `wasm32-unknown-unknown`) because it maps `std::fs` onto WASI-p2 interfaces that the host services. A bazel rule wraps this for hermetic, reproducible builds and emits the sha256 that goes in the manifest. There is **no existing rules_rust wasm-component rule in this repo today** (the `musl/` tooling cross-compiles the *host* binary, not guests), so the bazel guest-build rule is real new infra and is sized accordingly in the task breakdown (T9).

### Host runtime

- `wasmtime::component::bindgen!` generates host-side bindings from the WIT. The existing `DefaultExternalCheckExecutor` is rewritten to: load (or deserialize from cache) the component, build a per-invocation sandbox dir and wire it as the WASI root preopen (see file-capability section), instantiate into a `Store<HostState>` (today's store is `Store<()>` — it gains real state carrying the `WasiCtx`), call `list-checks`/`run-check`, and lift the returned `list<finding>` into the existing `Finding` type.
- The executor/provider/runner trait architecture (`ExternalCheckExecutor`, `ExternalCheckPackageProvider`, `CompositeExternalCheckPackageProvider`, the runner's `ExternalResolved` scheduling) is preserved. The change is concentrated in `runtime.rs` and the manifest schema; the wiring in `runner.rs` stays.

### File-capability model + allowlist policy

Checks read files with **ordinary `std::fs` APIs** — no checkleft-specific call, no special import. The `wasm32-wasip2` compilation target maps `std::fs` onto WASI-p2's filesystem interface, which `wasmtime` services on the host. Capability scoping is structural: the host controls what files the guest can reach by controlling what it preopens.

- **Primary mechanism: consume the shared FS sandbox module as the WASI preopen.** The shared FS sandbox module (§Shared FS sandbox module) accepts the changeset, the check's declared `access-scope` (`modified-only` by default), and the `SourceTree`, then produces a populated temp directory whose tree mirrors the allowlisted repo paths. The wasm runtime takes that directory and passes it to `WasiCtxBuilder::preopened_dir(sandbox_root, "/")`. The guest's `std::fs::read_to_string("src/foo.rs")` resolves through the WASI preopen into the sandbox dir — no custom ABI, no special function, just Rust. Path normalisation and `..` traversal rejection are applied inside the shared module; escape attempts fail because the sandbox dir simply does not contain the requested file.

- **Alternative: custom/virtual host FS.** If materializing files to disk per invocation proves too expensive, or if in-memory virtual trees (e.g. base-revision via `TreeVersion::Base`) cannot practically be materialized, the host can instead implement a `wasmtime_wasi::Dir` backed by the `SourceTree` trait and plug it in without writing to disk. This is more flexible (no temp-dir I/O) but more complex to implement. Treated as a fallback / future optimization rather than the v1 default.

- **Host-imported `read-file(path)` ABI: last resort only.** A WIT host function `read-file(path: string) -> result<list<u8>>` would break the ergonomic: authors write a checkleft-specific call instead of ordinary Rust. This approach must not be adopted unless **both** the sandbox-dir and the custom/virtual host-FS approaches are demonstrated to be unworkable — and any such decision requires explicit justification here for why they fail.

- **WASI linkage.** The host links `wasmtime-wasi` with the sandbox dir as the WASI root preopen. This single `WasiCtx` satisfies both file I/O and the WASI interfaces that `std` Rust guests pull in (clocks, stdio) — no separate stub is needed. (Open question Q4: sandbox-dir vs. custom/virtual host FS for the per-file enforcement.)

### Limit / timeout policy

- **Epoch-based deadlines** replace fuel as the default timeout. The engine enables epoch interruption; a background thread (or the existing run scheduler) ticks the epoch, and the store sets an epoch deadline. Default: generous wall-clock budget (proposed 5 s), far above the prototype's tight fuel ceiling.
- **Memory cap** via a `StoreLimits`/`ResourceLimiter` on the store (proposed default 256 MiB), configurable.
- **Per-check / per-bundle overrides** in the manifest (`limits.timeout_ms`, `limits.max_memory_mb`), clamped by a host ceiling so an out-of-tree manifest cannot grant itself unbounded resources. Trusted bundles can opt into a relaxed tier.
- Fuel remains available as an opt-in determinism knob (useful for reproducible CI) but is **off by default** in favor of epoch deadlines.

### Bundling / discovery format

- **One component, N checks**, self-describing via `list-checks`. The host instantiates the component once and enumerates `check-descriptor`s.
- **Manifest** evolves to a new `mode = "component"` / `runtime = "component-v1"`, carrying `artifact_path`, `artifact_sha256`, optional `limits`, and (optionally) an explicit `checks = [...]` allowlist that must agree with `list-checks` (defense in depth against an artifact silently exporting an unexpected check). The legacy `mode = "wasm"` / `runtime = "sandbox-v1"` is removed (see disposition).
- The bundled provider (`bundled.rs`) embeds component bytes via `include_bytes!` (today it `include_str!`s YAML manifests). A bundled component's exported checks become resolvable by name.

### AOT compilation + caching

- Precompile with `Engine::precompile_component` to a `.cwasm`, cached on disk keyed by **`(artifact_sha256, wasmtime_version, engine_config_hash, target_triple)`**. Load with `Component::deserialize_file` (trusted: checkleft produced the file). The cache key *must* include the wasmtime version because `.cwasm` is not portable across wasmtime releases — this is the central version-discipline risk (see Risks).
- Instantiate per-invocation in-process; no process spawn.
- **Cost story (to be measured in implementation, expected order-of-magnitude):**
  - Today (`sandbox-v1`): full `Module::new` compile **per invocation** — the dominant, repeated cost.
  - New: one-time compile (tens of ms) amortized into the `.cwasm` cache; subsequent runs pay deserialize (low ms) + instantiate (tens of µs) + the typed call. Net: repeated invocations get dramatically cheaper than the current recompile-every-time path.
  - vs. exec/declarative: avoids ~1-5 ms process spawn per invocation and OS-sandbox setup.

## Shared FS sandbox module

The per-invocation filesystem sandbox is designed as a **standalone, runtime-agnostic module**. It is not wasm-specific: the wasm runtime is the first consumer, but the declarative runtime (and any future exec runtime) can adopt it without any change to this module's interface. What differs across runtimes is only how they consume the sandbox root: the wasm runtime preopens it as the WASI root; a declarative runtime would pass it as a working directory or environment variable to a subprocess.

### Interface

**Inputs:**
- `changeset: &ChangeSet` — the set of files modified in this change.
- `scope: AccessScope` — the declared access scope (default: `ModifiedOnly`; see below).
- `source_tree: &dyn SourceTree` — the repository file tree, used to materialise content for virtual or git-backed trees.
- `ceiling: HostCeiling` — the host-enforced maximum (e.g. repo root, read-only); the module intersects the declared scope with this ceiling.

**Behavior:**
1. Resolve the **allowlist** from `scope`:
   - `ModifiedOnly` (default): allowlist = `changeset.changed_files` paths only.
   - `WholeRepo`: allowlist = all paths under the repo root, subject to `ceiling`.
   - `Globs(patterns)`: allowlist = files matching `patterns` (repo-root-relative) ∪ `changeset.changed_files`, intersected with `ceiling`.
2. Create a **per-invocation temp directory** and populate it with exactly the allowlisted files at their repo-relative paths via hardlinks (same filesystem) or by materialising content from `SourceTree` (virtual/git-backed trees).
3. Apply **path normalisation** (`validate_relative_path`) and reject `..` traversal during allowlist resolution; escape attempts fail structurally.

**Outputs:**
- `sandbox_root: TempDir` — the populated sandbox directory. The caller is responsible for its lifetime; dropping it removes the sandbox.
- `allowed_paths: Vec<RepoRelativePath>` — the materialised path list, for audit and logging.

### Default access policy: modified-only

**A check gets access only to the files it modified — by default.** `ModifiedOnly` is the safe, narrow default. Most checks inspect only the code being changed; granting the whole repo by default would silently give out-of-tree checks far broader access than they need.

**Whole-repo is opt-in.** A check must explicitly declare `access-scope = whole-repo` (in its `check-descriptor` WIT field or in its manifest TOML) to receive the full repository tree. The host may surface this declaration to reviewers as a higher-trust requirement.

**Globs are for targeted cross-file reads.** A check that needs specific supporting files beyond the changeset (e.g. `Cargo.toml` files alongside changed `.rs` files) declares `access-scope = { globs = ["**/Cargo.toml"] }`. Modified files are always included regardless of the glob list.

### Access scope declaration

Scope lives in the check's self-description:

- **WIT `check-descriptor`** (wasm tier): the `access-scope: option<access-scope>` field. Absent means `modified-only`.
- **Manifest TOML** (declarative tier, future): an `access_scope` key in the check descriptor block. Absent means `modified-only`.

This keeps scope collocated with the check definition, not hidden in a separate config layer.

### Wasm runtime consumption

The wasm runtime is the first consumer. After obtaining the sandbox root from this module, it calls:

```rust
WasiCtxBuilder::new()
    .preopened_dir(sandbox_root.path(), "/")
    .build()
```

The guest component reads files with ordinary `std::fs` — no checkleft-specific call, no WIT import. Capability enforcement is structural: the sandbox dir does not contain unauthorised files, so a path that was not allowlisted simply does not exist from the guest's perspective.

### Declarative runtime (future)

The declarative runtime is **not wired to this module yet** — that is explicitly out of scope for v1. When it adopts the module, it will pass the sandbox root to the subprocess as a working directory or environment variable. No change to this module's interface is required at that point; the shared module's contract is already general enough to serve both consumers.

## Changes to the T1407 check-def provider + CHECKS source directive

T1407 (PR 1402) gives us the bundled-def provider and the `check_definitions` CHECKS section (`exec_paths`, `allow_override_bundled`) — see `config.rs` (`ResolvedCheckDefinitions`) and `bundled.rs`. These **survive and are reworked**, not discarded:

- `ExternalCheckImplementationRef` (`File` / `Generated` / `Bundled`) is unchanged.
- **Component discovery.** A component artifact exports N checks. Resolution maps one component artifact to N logical `ExternalCheckPackage`s — one per exported check name — sharing the artifact and each carrying a `run-check` selector (the check `name`). Name-based resolution (bundled or exec-path) can therefore resolve `my-check` to "component X, export `my-check`."
- **Manifest loader.** `parse_external_check_manifest` / the TOML schema in `mod.rs` gains the `component` mode and `component-v1` runtime, and drops `wasm`/`sandbox-v1`. The `RawExternalCheckMode` enum gains `Component`; `validate_runtime_for_mode` maps it to `component-v1`.
- **Bundled provider.** `bundled.rs` embeds component bytes and resolves a `bundled:<name>` ref to the component package, with the enumerated check selected by name.
- **`capabilities.commands` is removed from the wasm/component tier** (it was inert there). `command_policy.rs` stays relevant only to the declarative tier's binary invocations.

## Disposition of prior work

- **T1397 / PR 1376 (`sandbox-v1` prototype): SUPERSEDE.** Delete the hand-rolled core ABI path (`checkleft_run`, manual `read_memory`/`write_memory`/`ensure_memory_capacity`/`decode_output_range`) and the fake `(string)->(string)` "component" path from `runtime.rs`. **Salvage:** sha256 pinning (`validate_artifact_sha256`), the `ExternalCheckExecutor`/provider/runner architecture, the manifest-parsing scaffolding, and the engine-construction skeleton. **There is no parallel dual-tier migration period:** `sandbox-v1` is removed immediately once the component path lands (T11). No new checks are registered against `sandbox-v1` after T7 merges.
- **T1444 / PR 1410 (`giant-struct-no-builder` ported to `sandbox-v1`): SUPERSEDE.** It is reference-only and built on the ABI being removed. Re-port the check onto the new component model as the end-to-end proof (T10), then close the old port at the same time T11 deletes `sandbox-v1`.
- **T1371 / PR 1372 (Rust-only restriction): KEEP.** It aligns with the Rust/wit-bindgen sweet spot and the v1 non-goal of multi-language guests.
- **T1407 / PR 1402 (bundled provider + CHECKS source directive): REWORK** to load the component format, as detailed above.

## Risks / open questions

- **Wasmtime version discipline (highest risk).** `.cwasm` artifacts are not portable across wasmtime versions, and the Component Model's host/guest ABI tracks the wasmtime/wit-bindgen release train. Mitigations: the cache key includes the wasmtime version (stale `.cwasm` is recompiled, never mis-loaded); bundled in-tree components are rebuilt as part of the normal build on a wasmtime bump; the workspace already pins wasmtime (`42.0.1`/`42.0.2`) in one place, so bumps are deliberate.
- **Component Model / `wit-bindgen` / `cargo component` tooling maturity.** These move faster than the Rust release train. Mitigation: pin guest tool versions in the bazel rule; keep the WIT contract small and stable (`@0.1.0`).
- **Bazel guest-build infra is new.** There is no rules_rust wasm-component rule in this repo today. Building reproducible `wasm32` components under bazel (hermetic toolchain, deterministic output, sha emission) is the largest single unknown; sized as `large` (T9).
- **WASI-p2 is the file-access path, not just a stub.** Because `wasm32-wasip2` maps `std::fs` onto WASI-p2 interfaces, the host must link `wasmtime-wasi` with a properly built `WasiCtx` (sandbox dir preopen). Any mismatch between the wasmtime-wasi version and the guest's WASI-p2 expectations surfaces at instantiation time. Mitigation: same workspace-level wasmtime pin that governs the component ABI.
- **Sandbox dir I/O cost.** Each invocation creates and tears down a temp dir with hardlinks/symlinks (or materialized content for virtual trees). On high-throughput CI runs this is measurable I/O. Mitigation: measure in T4; the custom/virtual host-FS alternative avoids disk writes entirely if profiling demands it.
- **Epoch timeouts are non-deterministic.** A wall-clock deadline can fire differently across machines. Acceptable given generous defaults; fuel is available as an opt-in knob for CI determinism.
- **Migration scope is small but real.** Only `giant-struct-no-builder` exists as a wasm-tier check today, so migration risk is low — but the proof must be genuinely end-to-end (authored via the SDK, built via the bazel rule, resolved via the provider, run via the new host).

All open questions are now resolved. Summary of decisions: (Q1) **resolved** — config crosses the WIT boundary as a JSON string (`config-json`); guest SDK deserializes with serde into the author's own struct; (Q2) **resolved** — WASI-p2 is central to the design and `wasmtime-wasi` is always linked; (Q3) **resolved** — epoch-based wall-clock deadline is the default; fuel is available as an opt-in determinism knob but is off by default; (Q4) sandbox-dir is the v1 default; custom/virtual host-FS is a deferred optimisation if profiling demands it (host-imported `read-file` ABI remains off the table); (Q5) **resolved** — `sandbox-v1` is removed immediately once the component path lands (T11), with no parallel dual-tier period.

## Build (bazel)

**Bazel is a hard requirement.** Both the wasmtime host runtime (checkleft's wasm executor) and the out-of-tree check components (guest modules) must be buildable entirely under bazel. "You can build it with cargo" is not an acceptable endpoint. "There is a shell script to build the check" is not an acceptable endpoint. The bazel build is the real, supported path for this project.

### Host / runtime side

The wasmtime host embedding (`tools/checkleft` and the new executor code) is standard Rust and builds under `rules_rust` with no new rule bodies:

- `rust_library` / `rust_binary` targets cover the executor rewrite, the `wasmtime::component::bindgen!` macro expansion (a proc-macro, handled at compile time inside `rules_rust`), the `HostState` / `WasiCtx` wiring, and the limit-policy code.
- The shared FS-sandbox module (T3a) is a pure-Rust `rust_library` with no wasm toolchain dependency; it builds like any other crate.
- The AOT `.cwasm` precompile step is a **runtime** operation performed on first component load against a live `Engine`. It is not a build-time artifact and requires no bazel rule; the cache directory is a runtime side effect managed by the executor.
- `wasmtime-wasi` is a normal Cargo dependency; `rules_rust` resolves it via the workspace `Cargo.lock`. No extra steps.

### Guest / check side: what `rules_rust` does NOT provide

**`rules_rust` ships a `rust_wasm_bindgen` rule. This is the wrong tool and must not be used.** `rust_wasm_bindgen` wraps the `wasm-bindgen` CLI, a JavaScript interop toolchain that targets `wasm32-unknown-unknown` for browser/Node consumption. It has no connection to WASI or the WebAssembly Component Model. Do not conflate the two.

Building a `wasm32-wasip2` Component Model component requires a pipeline that `rules_rust` does not cover end-to-end:

**Step 1 — Cross-compile to `wasm32-wasip2` with `rules_rust`.**
`rules_rust` supports arbitrary Rust target triples, including wasm ones, via a registered `rust_toolchain` that names the cross-compile triple. `wasm32-wasip2` is a tier-2 Rust target (available from Rust 1.78+) and is the preferred guest triple for Component Model checks because it targets WASI Preview 2 interfaces natively, reducing or eliminating the need for an adapter module. A `wasm_wasip2_toolchain` bazel setup — registering the `wasm32-wasip2` Rust cross-compiler triple and its `std` sysroot — must be written. This is toolchain configuration (a `toolchain()` target and a `platform()` constraint), not a new rule body, but it is new infra that does not exist in this repo today.

**Step 2 — Expose `.wit` files to the compiler sandbox.**
`wit-bindgen` operates as a proc-macro inside the guest SDK (`checkleft-check-sdk`). The macro is triggered by `#[check]` at compile time within `rules_rust`. For the macro to locate the WIT source files, those files must be declared as `data` on the relevant `rust_library` target and accessible inside the bazel sandbox at the path the macro expects (via `wit-bindgen`'s `world` path argument). This is a **sandbox visibility configuration**, not a new rule. The `.wit` package lives at a declared bazel target (e.g. `//tools/checkleft/wit:check_wit`), and the guest SDK target declares it as `data`.

**Step 3 — Componentization via `wasm-tools`.**
This is the step with no upstream `rules_rust` support. After Step 1, the output is a core WebAssembly module (or a WASI-p2 module, depending on the Rust target). Promotion to a signed Component Model component requires running `wasm-tools component new`. If targeting `wasm32-wasip2` directly, the adapter may not be required, but a `wasm-tools component embed` or `wasm-tools component new` invocation is still needed to embed the WIT package metadata and produce a standard component binary:

```
wasm-tools component new <core.wasm> -o <component.wasm>
# or, if targeting wasm32-wasip1 with an adapter:
wasm-tools component new <core.wasm> \
  --adapt wasi_snapshot_preview1=<wasip1-to-p2-adapter.wasm> \
  -o <component.wasm>
```

**A new custom bazel rule is required for this step.** The rule, provisionally named `rust_wasm_component`, chains the `rules_rust` compile output through a hermetic `wasm-tools` toolchain invocation and produces the final `.wasm` component as a declared bazel output.

**Step 4 — SHA-256 emission.**
The manifest's `artifact_sha256` field must be pinned at build time, not computed at runtime. The `rust_wasm_component` rule (or a sidecar genrule) emits a `<component>.wasm.sha256` text file alongside the component. CI stamps the digest into the CHECKS source at build time.

### Custom rules and toolchain declarations required

| Rule / Declaration | Purpose | Upstream support |
|---|---|---|
| `wasm_wasip2_toolchain` | Register the `wasm32-wasip2` Rust cross-compiler triple and its sysroot under bazel | Configuration targets — no new rule body, but must be written; does not exist in-repo today |
| `wasm_tools_toolchain` | Declare the `wasm-tools` CLI binary as a hermetic bazel toolchain, pinned to a specific version | New toolchain declaration |
| `rust_wasm_component` | Compile a Rust guest crate to wasm via `rules_rust`, then componentize via `wasm-tools`, and emit a sha256 sidecar | **New custom rule** — not in `rules_rust` or any upstream |
| `.wit` data exposure | Expose the `checkleft:check` WIT package as a declared bazel `data` dependency visible to the `wit-bindgen` proc-macro at compile time | `data` attribute on existing `rust_library` targets — no new rule |

The `rust_wasm_component` rule is the largest single piece of new bazel infrastructure. It encapsulates the cross-compile + componentize + sha256-emit pipeline into one bazel target, so a check author writes a single target declaration:

```python
rust_wasm_component(
    name = "my_check",
    srcs = glob(["src/**/*.rs"]),
    wit_package = "//tools/checkleft/wit:check_wit",
    deps = ["//tools/checkleft/sdk:checkleft_check_sdk"],
    visibility = ["//tools/checkleft:__pkg__"],
)
# Produces: my_check.wasm and my_check.wasm.sha256
```

### Scope of T9 (bazel rules for wasm component checks)

T9 in the task breakdown covers this entire "Build (bazel)" guest-side pipeline. It is sized `large` because it is genuinely new infrastructure with no prior art in this repo: a `wasm32-wasip2` toolchain registration, a hermetic `wasm_tools_toolchain`, and the `rust_wasm_component` rule body. T9 depends on T2 (guest SDK) so that the SDK shape is stable before the rule is written and the first check is built through it in CI. T9 gates T10 (end-to-end proof) but is otherwise parallel to T3-T8 and may proceed as soon as T2 lands.

The host-side bazel build (checkleft binary, executor, FS-sandbox module) is standard `rules_rust` and does not depend on T9. It lands incrementally alongside T3-T6 with no special gating.

## Migration plan

1. **Phase 0 — Design (this document).**
2. **Phase 1 — Contract + SDK.** Land the WIT package and the `checkleft-check-sdk` guest crate (T1, T2).
3. **Phase 2 — Host runtime.** New component executor with `host-fs` allowlist, epoch/memory limits, and `.cwasm` cache (T3-T6). Host-side bazel targets (`rules_rust`) land with the code; no custom rules needed.
4. **Phase 3 — Manifest/provider/CHECKS rework + bundling** (T7, T8).
5. **Phase 4 — Bazel rules for wasm component checks** (T9): `wasm32-wasip2` toolchain, `wasm_tools_toolchain`, and `rust_wasm_component` rule. This is first-class migration work, not an afterthought, and is on the critical path to the end-to-end proof.
6. **Phase 5 — Proof.** Port `rust-giant-structs-use-builder` onto the SDK, build it via the `rust_wasm_component` bazel rule (T9), resolve it via the provider, and run it through the new host with an end-to-end test (T10). The check must be built entirely under bazel — cargo is not the acceptance path. This is the proof for the whole project.
7. **Phase 6 — Cleanup.** Remove `sandbox-v1` runtime paths and the inert wasm-tier command-capability surface (T11).

## Proposed implementation task breakdown

Tasks are PR-sized and listed in dependency order. "Depth" notes which tasks share a dependency level and may run in parallel. Effort hint ∈ `trivial | small | medium | large`.

### T1 — WIT contract package
**Scope:** Add the in-tree `checkleft:check@0.1.0` WIT package (`types`, `host-fs`, `check` world) mirroring `input.rs`/`output.rs`. Includes the `access-scope` variant type and the updated `check-descriptor` record (with `access-scope: option<access-scope>` replacing the former `reads` field). Includes a host-side `bindgen!` smoke test that the WIT compiles and a doc comment mapping each WIT record to its Rust counterpart. No behavior wired yet.
**Effort:** small. **Depends on:** none.

### T2 — Guest SDK crate (`checkleft-check-sdk`)
**Scope:** New guest crate wrapping `wit-bindgen` with the `#[check(name, access_scope?)]` macro (defaulting to `modified-only` when no `access_scope` is given), the `list-checks`/`run-check` dispatch table, and a `CheckInput::config::<T>()` serde helper over `config-json`. Ships a trivial example check that compiles to a component. Authors see only native structs and never touch WIT bindings or `access-scope` unless they need broader access.
**Effort:** medium. **Depends on:** T1.

### T3 — Host component executor (typed call path)
**Scope:** Rewrite `DefaultExternalCheckExecutor` to load a component, build a `Store<HostState>` and `Linker`, instantiate, and call `list-checks`/`run-check`, lifting `list<finding>` into `Finding`. No file capability or cache yet (stub `host-fs` that denies all). Replaces the core/fake-component paths' call mechanics.
**Effort:** medium. **Depends on:** T1. *(Parallel with T2 — both depend only on T1.)*

### T3a — Shared FS sandbox module
**Scope:** Implement the standalone, runtime-agnostic FS sandbox module defined in §Shared FS sandbox module. Inputs: a changeset, a declared `AccessScope` (defaulting to `ModifiedOnly`), a `SourceTree` reference, and a `HostCeiling`. Behavior: resolve the allowlist per the declared scope (`ModifiedOnly` → changeset paths only; `WholeRepo` → all paths under ceiling; `Globs(patterns)` → glob expansion ∪ changeset, intersected with ceiling), create a per-invocation temp directory, populate it by hardlink (same filesystem) or by materialising content from `SourceTree` for virtual/git-backed trees, preserving repo-relative paths, and apply path normalisation plus `..` traversal rejection throughout. Outputs: the sandbox-root `TempDir` and the materialised path list. Unit tests for all three scope variants, traversal-escape rejection, and virtual-tree materialisation. No WASI, no wasm dependency — pure Rust, consumable by any runtime.
**Effort:** medium. **Depends on:** none. *(Pure Rust; can start immediately in parallel with T1.)*

### T4 — WASI integration: consume shared FS sandbox module as preopen
**Scope:** Wire the shared FS sandbox module (T3a) into the wasm executor: after T3a produces the sandbox root for the check's declared `access-scope`, call `WasiCtxBuilder::preopened_dir(sandbox_root, "/")` to create the `WasiCtx` and pass it into the `Store<HostState>`. The guest reads via `std::fs` with no checkleft-specific call; capability enforcement is structural. Integration tests for the full grant/deny/traversal-escape path exercised through a running wasm component.
**Effort:** small. **Depends on:** T3, T3a.

### T5 — Limit / timeout policy
**Scope:** Epoch-based deadline (engine epoch ticking + store deadline), memory `ResourceLimiter`, generous defaults, and manifest `limits.{timeout_ms,max_memory_mb}` overrides clamped by a host ceiling. Tests for timeout trip and memory cap trip.
**Effort:** medium. **Depends on:** T3. *(Parallel with T4 and T6.)*

### T6 — AOT precompile + `.cwasm` cache
**Scope:** `precompile_component` → on-disk `.cwasm` cache keyed by `(artifact_sha256, wasmtime_version, engine_config_hash, target)`, with safe deserialize-on-load and cache-miss rebuild. Benchmark cold vs. warm invocation to validate the cost story.
**Effort:** medium. **Depends on:** T3. *(Parallel with T4 and T5.)*

### T7 — Manifest schema for `component` mode
**Scope:** Add `mode = "component"` / `runtime = "component-v1"` to the TOML schema in `mod.rs` (`RawExternalCheckMode::Component`, `validate_runtime_for_mode`), carrying `artifact_path`, `artifact_sha256`, optional `limits`, optional `checks` allowlist. Remove the `wasm`/`sandbox-v1` mode and its now-inert `capabilities.commands` handling for this tier. Parser tests.
**Effort:** small. **Depends on:** T1. *(Parallel with T2/T3.)*

### T8 — Provider + CHECKS rework for component discovery
**Scope:** Map one component artifact to N logical packages (one per `list-checks` export, carrying a `run-check` selector); rework the bundled provider (`bundled.rs`) to `include_bytes!` component bytes; ensure name-based resolution (bundled + exec-path CHECKS `check_definitions`) resolves to the right component+export. Tests across composite-provider resolution.
**Effort:** medium. **Depends on:** T3, T7.

### T9 — Bazel rules for wasm component checks
**Scope:** All new bazel infrastructure required to build a `checkleft-check-sdk` guest crate into a `wasm32-wasip2` Component Model component under bazel end-to-end. This is first-class migration work, not an afterthought. Concretely:

1. **`wasm32-wasip2` toolchain registration.** Register the `wasm32-wasip2` Rust cross-compiler triple (and its pre-built `std` sysroot) as a `rust_toolchain` / `platform()` target pair under bazel. `rules_rust` supports arbitrary cross-compile triples; this is new toolchain configuration, not a new rule body, but it does not exist in-repo today.
2. **`wasm_tools_toolchain`.** Declare the `wasm-tools` binary as a hermetic bazel toolchain, version-pinned, downloadable via `http_file` or a registry rule. This is required for the componentization step and must be reproducible across CI machines.
3. **`rust_wasm_component` rule.** A custom Starlark rule that: (a) invokes `rules_rust`'s `rust_binary`-equivalent to compile to `wasm32-wasip2`, (b) passes the resulting `.wasm` through `wasm-tools component new` (with or without the wasip1 adapter, depending on the Rust target output), and (c) emits the final component `.wasm` and a `.wasm.sha256` sidecar as declared bazel outputs. The sha256 sidecar is what the CHECKS manifest pins.
4. **`.wit` data exposure.** Declare the `checkleft:check@0.1.0` WIT package as a bazel file target visible to the `wit-bindgen` proc-macro at compile time (via `data` on the guest SDK `rust_library` target). Verify that the bazel sandbox exposes the WIT files at the path the macro expects.
5. **CI smoke test.** Build the sample guest check (from T2) entirely through the new `rust_wasm_component` rule and verify the sha256 sidecar is produced. This confirms the pipeline is hermetic and reproducible before T10 depends on it.

**What this is NOT:** `rules_rust`'s `rust_wasm_bindgen` rule wraps the `wasm-bindgen` CLI for JavaScript interop targeting `wasm32-unknown-unknown`. It has no connection to WASI or the Component Model. Do not use it; the `rust_wasm_component` rule described above is a separate new rule.
**Effort:** large. **Depends on:** T2.

### T10 — Port `rust-giant-structs-use-builder` as the end-to-end proof
**Scope:** Re-author the check on the guest SDK (superseding the T1444 `sandbox-v1` port), build it via the `rust_wasm_component` bazel rule (T9) — **not via cargo, not via a shell script** — bundle/resolve it via T8, and run it through the new host (T3-T6) in an end-to-end test that exercises a capability-scoped file read with the default `modified-only` scope and a real finding. This is the project's acceptance proof; the proof is only valid if the check is built end-to-end under bazel.
**Effort:** medium. **Depends on:** T2, T3a, T4, T8, T9. *(T5/T6 should also be landed for a realistic run, but are not strictly gating the proof's correctness.)*

### T11 — Remove `sandbox-v1` runtime and dead capability surface
**Scope:** Delete the hand-rolled core ABI and fake-component paths from `runtime.rs`, the `sandbox-v1` constants, and the inert wasm-tier `capabilities.commands` plumbing. Update tests/docs. Strictly after nothing resolves to `sandbox-v1`.
**Effort:** small. **Depends on:** T8, T10.

### Parallelism summary (task graph, not a linear list)

- Depth 0: **T1** and **T3a** (both independent; T3a has no wasm or WIT dependency).
- Depth 1 (after T1, parallel): **T2**, **T3**, **T7**.
- Depth 2 (after T3, parallel): **T4** (needs T3 + T3a), **T5**, **T6**. (T9 also starts here, after T2.)
- Depth 3: **T8** (after T3, T7).
- Depth 4: **T10** (after T2, T3a, T4, T8, T9).
- Depth 5: **T11** (after T8, T10).

### Deferred / future — not a v1 blocker

- **Multi-language guests** (non-Rust SDKs). The WIT contract already permits it; no SDK ships in v1 (preserves T1371).
- **Custom/virtual host-FS (`wasmtime_wasi::Dir` backed by `SourceTree`).** If sandbox-dir I/O proves expensive at scale, replace disk materialization with an in-memory virtual dir. This also naturally supports base-revision reads without materializing to disk.
- **Base-revision (`TreeVersion::Base`) reads** — materialize base-revision content into the sandbox dir (or serve from the virtual host FS if that alternative is adopted).
- **End-to-end `SuggestedFix`/`FileEdit` application** from component findings (data already flows out; applying it is separate).
- **Remote component fetch + caching** beyond the existing external-URL provider path.
- **Instance pooling / warm-pool** for very high check counts (instantiation is already cheap; revisit only if profiling demands it).
- **Component signing / provenance** beyond sha256 pinning.

**Effort estimate (whole project):** ~11-13 PRs. The host-side path (T1, T3-T8, T3a, T10, T11) is mostly `small`/`medium` and well-understood given the existing architecture. T3a (shared FS sandbox module) is a standalone `medium` that can run fully in parallel with T1 and the rest of the wasm runtime work. The dominant unknown remains **T9 (bazel rules for wasm component checks, `large`)**: a `wasm32-wasip2` toolchain registration, a hermetic `wasm_tools_toolchain`, and a `rust_wasm_component` custom rule are all new bazel infrastructure with no prior art in this repo. T9 is on the critical path to T10 (the end-to-end proof) but is otherwise parallel to T3-T8 and can proceed as soon as T2 lands. The host-side bazel build (standard `rules_rust`) lands incrementally with T3-T6 and does not depend on T9.

## References

- [How do you build a Rust wasm binary with Bazel? (Stack Overflow)](https://stackoverflow.com/questions/78168400/how-do-you-build-a-rust-wasm-binary-with-bazel) — A starting-point reference for the bazel Rust→wasm build pipeline. **Caveat:** this question is from 2024 (~2 years old at time of writing); `rules_rust` and the wasm/Component-Model tooling have moved since then. Do not treat it as gospel — verify any approach it suggests against current `rules_rust` and `wasm32-wasip2`/Component-Model tooling before relying on it.
