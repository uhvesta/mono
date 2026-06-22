# Checkleft: `fix` subcommand (auto-apply fixes)

## Overview

`checkleft run` reports check failures; it never edits the tree. This design adds a companion `checkleft fix` subcommand that **automatically applies fixes** to the files a check is failing on. `fix` reuses `run`'s machinery to discover which files fail which checks, then drives each check's declared fix mechanism over its own failing-file set — a formatter's `--write`, a linter's `--fix`, a WASM check's fix entry point — writing the corrected bytes back to the real working tree.

The hard part is not invoking `prettier --write`; it is doing so **safely**, so that a fixer can only ever modify the files it was told to fix, never produces a partial write on error, and leaves the tree untouched when the fix fails. The chosen mechanism is a per-check **writable copy sandbox** with **atomic copy-back of only the changed files** — an airlock that makes "touch nothing outside the fixable set" a structural guarantee rather than a post-hoc check.

This document is a design only. No feature code is included; the final section is a dependency-ordered, PR-sized implementation breakdown.

## Goals

- Add `checkleft fix [PATHS…]` that applies fixes to every file failing a fixable check, as the write-side companion to `run`.
- Honor `--all` exactly like `run`: default change-scoped file set vs. full-repo scan.
- Add `--allow_dirty` (default **true**): whether it is acceptable to fix files that already have uncommitted modifications in the working tree. When false, do not fix already-dirty files (never clobber uncommitted work).
- Express the fix mechanism declaratively per check (a fix command in the check YAML) for the declarative tier, and as an SDK entry point for the WASM/external tier. Both are optional per check.
- Support batch and single-file fix modes, mirroring `run`'s `per_file`/`batch`.
- **Safety is the headline property:** a fix only ever writes files in its own fixable set; failures leave originals untouched; no partial writes.
- Provide a fix for as many pre-bundled checks as is feasible (all formatters, most linters).
- Deterministic, convergent results when multiple checks fix the same file.
- Re-verify after fixing and report what still fails (unfixable, or only partially fixed).
- Progress UI parity with `run` (fixing can be slow).

## Non-goals

- **Changing what `run` reports.** `run`'s output, exit semantics, and failing-file computation are unchanged; `fix` consumes them.
- **Interactive or partial fixing.** `fix` fixes _all_ failing fixable files by default. No per-hunk prompts, no "fix only finding #3". (Listed as `future / not a v1 blocker` in the task breakdown.)
- **Inventing fixes for checks that have none.** A failing check with no declared fix is a no-op for `fix` (reported as "no fix available"), never an error.
- **A general refactoring engine.** `fix` delegates to the check's own tool/logic; it does not implement fixes itself beyond applying the edits/writes those mechanisms produce.

## Current-state audit

Source: `tools/checkleft/src/main.rs`, `runner.rs`, `check.rs`, `output.rs`, `vcs.rs`, `config.rs`, `external/declarative/{mod,executor,resolve,selector}.rs`, `external/sandbox.rs`, `external/runtime.rs`, `external/component_bindings.rs`, `wit/check.wit`, `sdk/src/lib.rs`, `sdk-macro/src/lib.rs`, `bundled.rs`, and the `checks/{format,lint}/*.yaml` definitions.

### CLI / `run` dispatch (`main.rs`)

- Subcommands today: `Run(RunArgs)`, `List`, `ShowPlan` (temporary), `Install`, `Uninstall`. A bare `checkleft` is `run`.
- `RunArgs`: `--all: bool`, `--base_ref: Option<String>`, `--default_branch: Option<String>`, `--format human|json`, `--show_progress: Option<bool>`, plus `ConfigArgs { external_checks_file, external_checks_url }`. There is **no positional path arg** and **no `--allow_dirty`** today.
- `dispatch_run` resolves a `ChangePlan` from CI-env + overrides (`resolve_change_plan`), builds a `Runner`, resolves the `ChangeSet`, optionally wires a `LiveProgress` reporter, calls `runner.run_changeset_with_progress`, sorts results, renders human/JSON, and exits `1` iff any finding is `Severity::Error`.

### Runner orchestration (`runner.rs`)

- `Runner { registry, resolver, source_tree, external_package_provider, external_executor }`.
- `run_changeset_with_progress(changeset, reporter)` calls `schedule_runs(changeset)` to dedupe checks into `ScheduledCheckRun { configured_check_id, source_path, execution, policy, config, changeset }`, where `changeset` is **already filtered to the files that check applies to**.
- `ScheduledExecution` is one of `BuiltInConfigured { check }`, `BuiltInMissing`, `ExternalResolved { package }` (declarative **and** WASM both flow through `external_executor`), `Invalid`.
- Checks run concurrently in a `JoinSet`; built-ins via `ConfiguredCheck::run_with_progress`, externals via `spawn_blocking` → `ExternalCheckExecutor::execute_with_progress`.
- Each produces `CheckResult { check_id, findings: Vec<Finding> }`. `scope_findings_to_changeset` drops findings on files outside the changed set (a no-op under `--all`).
- **The failing-file set of a check is exactly the set of distinct `finding.location.path` values in its `CheckResult`** (filtered to error/warning as appropriate). This is the join point `fix` reuses — no new "which files fail" logic is needed.

### Check + finding model (`check.rs`, `output.rs`, `sdk/src/lib.rs`)

- `Finding { severity, message, location: Option<Location>, remediations: Vec<String>, suggested_fix: Option<SuggestedFix> }`.
- `SuggestedFix { description, edits: Vec<FileEdit> }`, `FileEdit { path: PathBuf, old_text: String, new_text: String }`.
- **These already exist** and are surfaced in the human renderer (`= fix: …`), but are currently a _latent, unapplied_ channel — no code writes them back. `fix` can adopt them as a fix source for built-in checks (see Chosen approach §F).
- The WIT contract (`wit/check.wit`) already defines `file-edit` and `suggested-fix` records and carries `suggested-fix: option<suggested-fix>` on `finding`. The guest SDK exposes `FileEdit`/`SuggestedFix` types. There is **no fix entry point** today and the guest FS preopen is **read-only**.

### Declarative executor (`external/declarative/`)

- A check is `ExternalCheckDeclarativePackage { applies_to, needs, invocations, skip_symlinks }`.
- `Invocation { id, kind }`; `InvocationKind::Tool(ToolInvocation { run, mode: InvocationMode (Batch|PerFile), args })` or `BazelAspect(...)`.
- Args are templated: `{{files}}` (batch → matched file list), `{{file}}` (per-file), `{{repo_root}}`, `{{config.KEY}}`. Argv is chunked under a 128 KiB threshold.
- `ExitSemantics { codes: BTreeMap<i32, ExitOutcome>, default }` with `ExitOutcome::{Ok, Findings, Error}`. `Error` must never be masked as clean.
- Invocations run with **cwd = repo root**, directly on real files; **declarative checks are not sandboxed today** (sandboxing "deferred by design"). The tool's stdout is parsed by a `transform` (`linelist | json | passthrough`) into findings. The tool is run in **check mode** (`--list-different`, `--check`, `-mode=check`) and never writes.
- The bundled snapshot (`bundled.rs`) embeds each YAML via `include_str!`, so adding fields to a YAML automatically flows into the compiled-in defaults.

### Sandbox (`external/sandbox.rs`)

- `create_sandbox(changeset, scope: AccessScope, source_tree, ceiling: &HostCeiling) -> Result<SandboxResult>`; `SandboxResult { root: TempDir, allowed_paths: Vec<PathBuf> }`.
- `AccessScope::{ModifiedOnly, WholeRepo, Globs(Vec<String>), ExplicitFiles(Vec<PathBuf>)}`.
- Populates the temp dir at repo-relative paths, **preferring `fs::hard_link` from the ceiling** (zero-copy) and falling back to `source_tree.read_file` (copies) for cross-filesystem / virtual-tree / symlink entries. Path normalization rejects `..` escapes; symlinks are always materialized via `SourceTree` (containment-checked).
- Today the sandbox is consumed **read-only** by the WASM runtime (`WasiCtxBuilder…preopened_dir(root, "/")`, read-only). **The hardlink fast path is the critical hazard for fix:** a hardlink shares an inode with the real file, so an in-place truncating write inside the sandbox would silently mutate the real file outside any copy-back control.

### WASM runtime + SDK (`external/runtime.rs`, `component_bindings.rs`, `wit/check.wit`, `sdk*`)

- Component Model is landed: `wasmtime::component::bindgen!` over `wit/check.wit`; world `check` exports `list-checks`, `run-check(name, input) -> result<list<finding>, check-error>`, plus optional `declare-required-files` / `declared-exclusions` / `evaluate-exclusion`.
- Epoch-based timeouts (base 5 s, +100 ms/file, host ceiling 5 min), `MemoryLimiter` (default 256 MiB, ceiling 512 MiB), AOT `.cwasm` cache. Phase-1 store (no preopen) does discovery; phase-2 store preopens the sandbox root read-only.
- Guest SDK: `#[check(name, description?, severity?, access_scope?, declared_exclusions?, evaluate_exclusion?, required_files?)]` + `export_checks!`. Author writes `fn(CheckInput) -> Vec<Finding>`. The `CheckEntry` trait (`__private`) is the host-facing dispatch surface; **it has no `fix` method**.

### VCS (`vcs.rs`)

- `Vcs { root, kind: Git|Jujutsu }`. `current_changeset` (working-tree vs HEAD), `changeset_since(base)` (merge-base diff), `all_files_changeset` (tracked files, for `--all`).
- **No explicit working-tree dirty query exists yet** — `fix` adds one (`git status --porcelain` / `jj` equivalent) for `--allow_dirty`.

## Alternatives considered (safety mechanism)

The contract proposes sandbox-copy-back and asks us to evaluate alternatives. Three were weighed.

### A. In-place fix + post-verify that only allowed files changed (rejected)

Run the fixer directly on the real tree (as `run` invokes tools today), snapshot the fixable files' pre-content first, then after the run diff the tree and assert nothing outside the fixable set changed; roll back from the snapshot on violation.

Rejected:

- **Rollback is best-effort, not atomic.** To roll back you must have snapshotted _every_ file the tool _might_ touch — but the whole risk is that the tool touches files you did not anticipate. You cannot snapshot the complement of a set you do not know.
- **A crash mid-write leaves real damage.** If the process dies after the tool has rewritten three files and before verification, the tree is in a half-fixed state with no airlock to discard.
- **Detection ≠ prevention.** "Assert afterward that nothing escaped" still _let the escape happen_; for a buggy formatter that rewrites a sibling import, the damage is already on disk.

### B. Per-check writable copy sandbox + atomic copy-back of changed files (CHOSEN)

Stage only the fixable files into a fresh temp dir (forced copies, never hardlinks), run the fixer with cwd = sandbox, detect which staged files changed, and atomically copy _only those_ back to the real tree. Detailed in Chosen approach.

Chosen because the safety property is **structural**: the fixer can write anywhere it likes _inside the sandbox_, but the host only ever copies back paths that were in the fixable set to begin with, and discards the entire sandbox on any error. "Touch nothing outside the fixable set" is then a property of the copy-back loop's domain, not of the tool's good behavior. It also reuses the existing `create_sandbox` infrastructure (`AccessScope::ExplicitFiles`, path-containment, `SourceTree` materialization) almost verbatim — the only new primitive is "force copy, no hardlink" plus the copy-back.

### C. One shared sandbox for the whole run (rejected as the default)

Stage every fixable file for every check into a single sandbox, run all fixers, copy back once.

Rejected as the unit of isolation:

- **Cross-check interference.** Two checks fixing the same file in one shared dir race or clobber each other; ordering (lint-before-format, §6) can no longer be enforced by sequencing copy-backs.
- **Coarse failure blast radius.** If one fixer errors, you must decide the fate of every other check's staged edits at once.
- A per-check (more precisely, per-fix-invocation) sandbox keeps each fixer's blast radius to its own files and lets the scheduler order and serialize overlapping fixers cleanly. (We _do_ reuse one sandbox across the files of a single check's batch invocation — that is the batch, not the cross-check, case.)

## Alternatives considered (WASM fix shape)

### W1. Guest returns edits; host applies them (CHOSEN)

Add a `fix-check(name, input) -> result<list<file-edit>, fix-error>` export. The guest reads its (read-only) sandbox, computes `file-edit` records (the record type **already exists** in the WIT), and returns them. The host validates every `edit.path ∈ fixable set` and applies edits to real files through the same atomic write path as the declarative tier.

Chosen because: it needs **no new write capability** in the WASI sandbox (guest FS stays read-only — a smaller trust surface); it reuses the `file-edit`/`suggested-fix` types already in `wit/check.wit` and the SDK; "only touch fixable files" is enforced by the host before a single byte is written; and it matches the ergonomic the SDK already implies (pure function over `CheckInput`). It is also testable without a filesystem.

### W2. Writable sandbox preopen; guest writes files; host copies back (alternative)

Give the fix invocation a **writable** WASI preopen (the forced-copy sandbox from approach B) and let the guest mutate files with `std::fs::write`; the host then copy-backs changed files exactly like the declarative tier.

This is the literal reading of the contract's "capability for the wasm check to write the (sandboxed) file(s)." It is fully supported by the same safety core (B) and is the right escape hatch for a guest that wraps native formatting logic which only knows how to rewrite a file in place. It is **not** the v1 default because it widens the guest capability (WASI write) for no benefit over W1 in the typed-Rust-check sweet spot, and edits are easier to validate, log, and unit-test than opaque file writes. **Decision surfaced for human confirmation** (see attentions + Risks): ship W1 as the SDK default, keep W2 as a declared opt-in (`#[check(fix = …, writable)]`), or require W2 outright.

## Chosen approach

### A. CLI surface and relationship to `run`

```
checkleft fix [PATHS…] [--all] [--allow_dirty[=true|false]]
              [--base_ref <ref>] [--default_branch <name>]
              [--format human|json] [--show_progress[=BOOL]]
              [--verify[=BOOL]] [--max_passes <n>]
```

- **Discovery shares `run`.** `fix` resolves the same `ChangePlan` (honoring `--all`, `--base_ref`, `--default_branch`, and `PATHS…`), builds the same `Runner`, and calls the existing run path to obtain `Vec<CheckResult>`. For each check, the **failing-file set** is the distinct `finding.location.path` values whose severity is `Error` or `Warning` (info findings are advisory and not fixed). No new per-check applicability/failure logic is introduced.
- **`PATHS…`** further intersect the candidate set with the given paths (a convenience for "just fix this dir"); absent, behavior matches `run`.
- **Apply phase.** For each check with a declared fix and a non-empty failing set, schedule a _fix run_ (§B). Checks with no declared fix are recorded as "no fix available" and skipped.
- **Output** (`--format human` default; `json` mirrors structure):
  - **Fixed:** per check, the files written (count + list).
  - **Still failing:** files that remained failing after fix + re-verify (unfixable check, or partially fixed).
  - **No fix available:** checks that failed but declare no fix, with their failing files (so the user knows what to fix by hand).
  - A summary footer (files fixed, files still failing, checks skipped) and elapsed time, matching `run`'s footer style.
- **Exit code.** `0` when, after fixing and re-verifying, **no `Error`-severity finding remains**; `1` otherwise. (So `fix` that fully resolves a previously-failing tree exits 0; one that leaves unfixable errors exits 1, like `run` would.) Operational failures (a fixer that errored, a sandbox failure) are `Error` findings and thus also exit 1.

### B. Safety core — writable copy sandbox + atomic copy-back

This is the load-bearing mechanism; every fix (declarative tool, WASM W2, and even the apply step of W1/§F) funnels through it.

For each fix invocation over a fixable set `F` (repo-relative paths):

1. **Compute `F`** = (failing files of the check) ∩ (`applies_to`) ∩ (`--allow_dirty` filter, §E) ∩ (`PATHS…`). If `F` is empty, the invocation is a no-op.
2. **Stage a writable sandbox** containing exactly `F`. Reuse `create_sandbox` with `AccessScope::ExplicitFiles(F)` **plus a new "force-copy" flag** so files are always `fs::copy`'d, **never hardlinked** (hardlinks share inodes — an in-place write would escape copy-back; see audit). Record, for each staged file, a **pre-fix content hash** (e.g. blake3) and its mode.
3. **Run the fixer** with cwd = sandbox root:
   - _Declarative:_ the fix invocation's tool + args (§C), batch or per-file, files passed as sandbox-relative paths via `{{files}}`/`{{file}}`.
   - _WASM W2:_ preopen the sandbox **writable**, call `fix-check`.
   - _WASM W1 / suggested_fix (§F):_ the "fixer" produces `file-edit`s; apply them to the staged sandbox copies.
4. **Classify the result** via the fix's exit/`fix-error` semantics (§C/§D). On `error` → **abort: drop the sandbox, real tree untouched.** Report an `Error` finding for the check.
5. **Detect changed files**: re-hash every staged file; the changed set `C` = files whose hash differs. **Enforce the airlock:** `C ⊆ F` by construction (only `F` was staged and only staged paths are walked); any file the fixer _created_ in the sandbox outside `F` is simply never enumerated for copy-back and dies with the temp dir. Newly-created paths and deletions inside the sandbox are logged but not propagated (a fixer must not create/delete files; doing so is reported, not applied).
6. **Atomic copy-back** of each `c ∈ C`: write the new bytes to a temp file in the **same directory** as the real target (same filesystem → atomic `rename`), preserving mode, then `rename` over the target. Per-file atomicity is guaranteed by POSIX `rename`. Across multiple files there is no kernel multi-file transaction; we copy back in a deterministic order and, **on the first copy-back I/O error, stop and report exactly which files were applied** (the already-renamed ones are valid, complete files — never half-written). The sandbox is dropped last.

**Failure handling guarantees:**

- A fixer that exits error → zero writes to the real tree.
- A crash during the fixer run → the real tree is untouched (all work was in the sandbox).
- A crash during copy-back → each individual file is either its original or its fully-fixed version (atomic rename), never a partial mix within a file.

### C. Declarative fix-command schema

The fix is expressed as an **optional `fix` block on an invocation**, backward-compatible (absent ⇒ that invocation has no fix). It mirrors the existing `ToolInvocation` shape so the same binary resolution (`needs`), templating, and chunking apply.

```yaml
invocations:
  - id: format
    run: oxfmt
    mode: batch
    args: ["--list-different", "{{files}}"] # unchanged CHECK invocation
    exit: { "0": ok, "1": findings, "2": findings, default: error }
    transform: { kind: linelist, message: "file needs oxfmt formatting" }
    fix: # NEW — optional
      # `run` defaults to the invocation's `run` (same binary); override if needed.
      mode: batch # batch | per_file; defaults to the invocation's mode
      args: ["--write", "{{files}}"] # FIX args (write/fix flags)
      # Exit semantics for the FIX run. Outcomes: `ok` (fix applied / nothing to do)
      # or `error` (fix failed → abort, no copy-back). Defaults: 0 => ok, else error.
      exit: { "0": ok, default: error }
```

Schema details:

- **`fix.run`** (optional): binary key into `needs`; defaults to the invocation's `run`. (Almost always the same tool with different args.)
- **`fix.mode`**: `batch` (one process over `{{files}}`) or `per_file` (one process per `{{file}}`); defaults to the invocation's `mode`. Per-file fix isolates a bad file (one file's fix error does not abort the batch), matching `run`'s per-file isolation.
- **`fix.args`**: templated like check args (`{{files}}`, `{{file}}`, `{{repo_root}}`, `{{config.KEY}}`). Convention: the fix args are the check args with the report flag swapped for the write flag (`--list-different`→`--write`, `--check`→(removed), `-mode=check`→`-mode=fix`).
- **`fix.exit`**: `ExitOutcome` reduced to `{Ok, Error}` (a fix has no "findings"). Default `0 ⇒ ok`, else `error`. A formatter's `--write` exits 0 on success; a linter's `--fix` may exit non-zero when _unfixable_ diagnostics remain — that is **not** a fix error (the fixable ones were still applied); model this by mapping the linter's "fixed-but-residual" code to `ok` and letting the post-verify (§G) report the residue. Per-check exit maps make this explicit.
- **What counts as a successful fix:** exit maps to `ok` **and** the sandbox airlock + copy-back complete without I/O error. Whether the file ended fully clean is decided by §G re-verify, not by the fixer's exit alone.
- `bazel_aspect` invocations (clippy) have **no `fix` block** in v1 (see §H, lint/rust).

The bundled YAMLs gain `fix` blocks; because `bundled.rs` embeds them via `include_str!`, the compiled-in defaults update automatically.

### D. WASM/external fix entry point

Add to `wit/check.wit` world `check`:

```wit
variant fix-error { unknown-check(string), failed(string), not-fixable }
export fix-check: func(name: string, input: check-input) -> result<list<file-edit>, fix-error>;
```

- **SDK:** extend `#[check(...)]` with an optional `fix = fn_name` argument; the fixer has signature `fn(CheckInput) -> Vec<FileEdit>` (or `Result<Vec<FileEdit>, String>`). Add `fn fix(&self, input: CheckInput) -> FixResult` to the `__private::CheckEntry` trait (default: `not-fixable`), wired by the macro and dispatched by `export_checks!`. A check without `fix = …` reports `not-fixable` and is a no-op for `fix`.
- **Runtime invocation:** the host reuses the existing phase-2 component instantiation. For **W1 (default)** the sandbox preopen stays **read-only**; the guest returns `file-edit`s; the host validates each `edit.path ∈ F`, applies them to the staged sandbox copies (or directly via §F apply), then copy-backs (§B). For **W2 (opt-in)** the host builds the sandbox **writable**, calls `fix-check` (the guest mutates files via `std::fs`), then detects-and-copies-back. Both paths share the §B core.
- **Capability for the WASM check to write:** under W1 the guest needs _no_ write capability (it returns data); under W2 the host grants a writable WASI preopen scoped to the forced-copy sandbox — the only files it can reach are `F`, and only changed ones are copied back.

### E. `--allow_dirty` (default true)

- **Dirty detection.** Add `Vcs::dirty_paths() -> Result<HashSet<PathBuf>>`: `git status --porcelain` (paths with worktree modifications, staged or unstaged, plus untracked) for Git; the `jj` working-copy diff for Jujutsu. Repo-relative, normalized to match changeset paths.
- **`--allow_dirty=true` (default):** dirty files are eligible to be fixed. This is the common local workflow ("I have uncommitted edits; format them").
- **`--allow_dirty=false`:** subtract `dirty_paths()` from every check's fixable set `F` _before_ staging. Skipped-because-dirty files are reported in the output (distinct from "no fix available"), so the user knows why they were left alone. No file is ever staged or written.
- **Interaction with `--all`:** `--all` only widens the _candidate_ set (full repo vs. change-scoped); the dirty filter is applied identically afterward. Note a deliberate consequence: in **local change-scoped mode** the failing set _is_ the working-tree diff, i.e. dirty files, so `--allow_dirty=false` there fixes essentially nothing — which is the point: it is the mode for "only fix already-committed/clean files" (e.g. a CI auto-fix bot on a clean checkout) without touching a human's in-flight edits. This is documented in `userdoc/cli.md`.

### F. Built-in checks and `Finding.suggested_fix`

Built-in (Rust) checks have neither a declarative `fix` block nor a WASM entry point, but `Finding.suggested_fix: Option<SuggestedFix>` **already exists** as an unapplied edit channel. `fix` adopts it as a third fix source: when a built-in check's finding carries `suggested_fix.edits`, the host treats those `FileEdit`s as the fixer output and runs them through the §B apply+copy-back path (validating `edit.path ∈ F`). This gives built-ins a zero-new-surface fix path: a check author simply populates `suggested_fix` on the findings they know how to repair (e.g. `no-usfa-typo` could emit the corrected spelling). v1 wires the _application_ of `suggested_fix`; populating it on specific built-ins is incremental follow-up (task breakdown marks per-check authoring as future).

### G. Ordering, convergence, concurrency, verification

- **Deterministic cross-check order (same file fixed by multiple checks).** When a file is in the fixable set of more than one check, fixes are applied in a fixed category order: **lint-fix before format-fix, format-fix last.** Rationale: a linter's `--fix` can insert or rewrite code (producing unformatted output), so formatting must run last to normalize it; the reverse can leave a formatted file un-formatted again. Within a category, order is stable by check id. (Concretely for a `.ts` file: `lint/oxc --fix` → `format/oxc --write`.)
- **Concurrency.** Build a conflict graph keyed by fixable-file overlap. Checks with **disjoint** fixable sets run **concurrently** (independent sandboxes, independent copy-backs — no shared real files). Checks that **share** any file are **serialized** in the category order above, each re-reading the latest real bytes (so the second check sandboxes the output of the first). This makes concurrent fixes provably safe (concurrent ⇒ disjoint).
- **Convergence.** Default is a single ordered pass (lint→format) per file, which is stable in practice for the bundled checks. `--max_passes <n>` (default 1, or 2 if we choose fixpoint-by-default — open question) re-runs the ordered fix pipeline on files that still change, up to `n`, stopping early when a pass produces no change. A hard cap prevents oscillation between two non-converging fixers.
- **Verification / idempotency (`--verify`, default on).** After fixing, re-run the _check_ logic (the `run` path) over the fixed files and report any residual findings as "still failing." This confirms resolution, surfaces partially-fixed files, and drives the exit code (§A). `--verify=false` skips the re-run for speed.

### H. Per-check fix coverage

| Check             | Tier                       | Fix mechanism                                      | v1        |
| ----------------- | -------------------------- | -------------------------------------------------- | --------- |
| `format/oxc`      | declarative                | `oxfmt --write {{files}}` (batch)                  | ✅        |
| `format/prettier` | declarative                | `prettier --write {{files}}` (batch)               | ✅        |
| `format/biome`    | declarative                | `biome format --write {{files}}` (batch)           | ✅        |
| `format/rust`     | declarative                | `rustfmt {{file}}` (per_file; drop `--check`/`-l`) | ✅        |
| `format/bazel`    | declarative                | `buildifier -mode=fix {{files}}` (batch)           | ✅        |
| `lint/oxc`        | declarative                | `oxlint --fix {{files}}` (batch)                   | ✅        |
| `lint/biome`      | declarative                | `biome lint --write {{files}}` (batch)             | ✅        |
| `lint/js`         | declarative                | `eslint --fix {{files}}` (batch)                   | ✅        |
| `lint/bazel`      | declarative                | `buildifier -lint=fix -mode=fix {{files}}` (batch) | ✅        |
| `lint/rust`       | declarative (bazel_aspect) | `clippy --fix` — **not feasible in v1**            | ❌ future |

`lint/rust` is **not auto-fixed in v1**: clippy is run via the rules*rust \_aspect* (artifact read), not a direct invocation, and `cargo clippy --fix` requires a cargo-driven, writable, single-version workspace and rewrites whole crates (well outside the changeset-scoped fixable set). Deferred (`future / not a v1 blocker`); failing `lint/rust` is reported as "no fix available."

**Not auto-fixable (semantic / intentional no-fix), reported as "no fix available":** `file/size`, `todo-expiry` (`todo_expiry.rs`), `no-usfa-typo` (`typo.rs`)*, `repo_visibility`, `forbidden_imports_deps`, `frontend_no_legacy_api`, `rust_test_rule_coverage`, `workflow_action_version` / `workflow_run_patterns` / `workflow_shell_strict`, `code_patterns`, `api-breaking-surface`, `link-integrity`, `ifchange-thenchange`. (*Several of these — `no-usfa-typo` especially — _could_ emit a `suggested_fix` later via §F; that is future per-check authoring, not a framework change.)

### I. Progress UI integration

`fix` reuses the existing `ProgressReporter` / `LiveProgress` / `TermRenderer`. The discovery phase reuses `run`'s reporter as-is. The apply phase registers one progress line per scheduled fix run (`register(check_id, file_count)`), ticks per file fixed (`record_progress`) — `create_sandbox`'s parallel population and per-file fix mode both expose natural tick points — and `finish`es with the count copied back. Auto-enabled on an interactive TTY, off for pipes/CI/`--format json`, identical detection to `run`. The final footer is the §A summary.

## Risks / open questions

- **WASM fix shape (W1 vs W2).** Recommendation: ship **W1 (guest returns edits, read-only sandbox)** as the SDK default and keep **W2 (writable sandbox)** as an opt-in. The contract's wording leans toward a write capability (W2). Needs a human call — surfaced in attentions.
- **Convergence default.** Single ordered pass (`--max_passes 1`) vs. converge-to-fixpoint (default 2+). A single lint→format pass is stable for all bundled checks; fixpoint costs extra re-runs for safety against pathological fixers. Surfaced in attentions.
- **Exit code when unfixable errors remain.** Proposed: exit `1` if any `Error` finding survives re-verify (consistent with `run`). An auto-fix bot might prefer `fix` to exit `0` whenever it _applied_ something regardless of residue. Surfaced in attentions.
- **`eslint --fix` / `lint/js` provisioning.** `lint/js` is being phased out in favor of `lint/oxc` (#1619). Confirm we still wire its fix, or drop it from the v1 coverage table.
- **Forced-copy cost.** Fix forfeits the hardlink fast path (correctness requires copies). For large `--all` fixes this is more I/O than `run`; acceptable because fix sets are usually small, but worth measuring.
- **`fix.run` override surface.** Defaulting `fix.run` to the check's `run` covers every bundled tool; we expose an override for completeness but should confirm no bundled check actually needs a _different_ binary for fix vs. check.
- **Renames / generated files.** A fixer that wants to _rename_ or _create_ a file (rare for formatters/linters) is intentionally unsupported by copy-back (only in-place content edits propagate). Confirm no target check needs this in v1.

## Proposed implementation task breakdown

Dependency-ordered, PR-sized. Effort hints: `trivial | small | medium | large`. Parallelism noted per depth.

**Depth 0 — may run in parallel (no inter-dependencies):**

- **T1. `fix` CLI surface + discovery wiring**
  Scope: Add the `Fix(FixArgs)` subcommand and `dispatch_fix` to `main.rs`. `FixArgs` mirrors `RunArgs` plus `--allow_dirty` (default true), `--verify` (default true), `--max_passes`, and positional `PATHS…`. Resolve the `ChangePlan`/`Runner` exactly like `run`, execute the run path, and compute per-check failing-file sets from `finding.location.path`. In this PR the apply phase is a **dry plan** that prints what _would_ be fixed (no writes), so the discovery half is reviewable in isolation.
  Effort: medium. Dependencies: none.

- **T2. Safety core: writable copy sandbox + atomic copy-back**
  Scope: Add a force-copy mode to `create_sandbox` (never hardlink) and a new module that, given a fixable set, stages a writable sandbox, records pre-fix content hashes, detects changed files post-fix, enforces the `C ⊆ F` airlock, and copy-backs via same-dir temp + atomic `rename` (mode-preserving), with first-error-stop reporting and full sandbox discard on failure. Pure mechanism + unit tests; no fixers wired yet.
  Effort: large. Dependencies: none.

- **T3. Declarative `fix` schema (parse + validate)**
  Scope: Extend `RawInvocation`/`Invocation` with an optional `fix` block (`run?`, `mode?`, `args`, `exit`) reducing `ExitOutcome` to `{Ok, Error}`; validate backward-compatibly (absent ⇒ no fix). Model only — no execution.
  Effort: medium. Dependencies: none.

**Depth 1 — may run in parallel once their deps land:**

- **T4. Declarative fix executor**
  Scope: Execute a parsed `fix` block inside the T2 sandbox: resolve the binary via `needs`, template/chunk args, run batch or per-file with cwd = sandbox, classify via `fix.exit`, hand changed files to copy-back. Wire into T1's apply phase, replacing the dry plan for declarative checks.
  Effort: medium. Dependencies: T2, T3, T1.

- **T5. Author `fix` blocks for bundled declarative checks**
  Scope: Add `fix` blocks to `checks/format/{oxc,prettier,biome,rust,bazel}.yaml` and `checks/lint/{oxc,biome,js,bazel}.yaml` per the coverage table; refresh the bundled snapshot. No Rust changes.
  Effort: small. Dependencies: T3. (Parallel with T4; T4 needed to _test_ end-to-end.)

- **T6. `--allow_dirty` + dirty-file detection**
  Scope: Add `Vcs::dirty_paths()` (git/jj), thread `--allow_dirty` through the fixable-set computation (subtract dirty when false), and report skipped-because-dirty distinctly.
  Effort: small. Dependencies: T1.

- **T9. WASM/external fix entry point**
  Scope: Add `fix-check` + `fix-error` to `wit/check.wit`; add `fix = fn` to `#[check]` and `fix(...)` to `CheckEntry`; dispatch via `export_checks!`; host runtime invokes `fix-check` (W1 read-only + host-applied edits as the default), routing through T2 copy-back. (W2 writable preopen gated behind the attentions decision.)
  Effort: large. Dependencies: T2. (Parallel with the declarative track.)

**Depth 2 — may run in parallel:**

- **T7. Ordering, conflict-graph scheduling, convergence**
  Scope: Build the fixable-file conflict graph; run disjoint checks concurrently and overlapping checks serially in category order (lint→format); implement `--max_passes` re-run-until-stable with a hard cap.
  Effort: medium. Dependencies: T4.

- **T8. Verification + output (human/JSON)**
  Scope: Implement `--verify` re-run over fixed files; render the three result buckets (fixed / still failing / no-fix-available) and summary footer in human + JSON; set the exit code per §A.
  Effort: medium. Dependencies: T4, T1.

**Depth 3 — may run in parallel:**

- **T10. Built-in fix via `Finding.suggested_fix`**
  Scope: Apply existing `suggested_fix.edits` from built-in checks as a fix source through the T2 copy-back path (path-validated). Framework wiring only; no per-check authoring.
  Effort: small. Dependencies: T2, T8.

- **T11. Progress UI for fix**
  Scope: Wire the apply phase into `ProgressReporter`/`LiveProgress` (register/tick/finish per fix run), with the same auto-detect rules as `run`.
  Effort: small. Dependencies: T1, T8.

- **T12. Safety + behavior test suite**
  Scope: Tests proving: only-fixable-files are written (sandbox-escape attempt is contained); `--allow_dirty` true vs. false; a check with no fix is a no-op; atomicity — abort on fixer error leaves originals byte-identical; copy-back first-error-stop never half-writes a file; deterministic lint→format ordering; idempotency (second `fix` is a no-op).
  Effort: medium. Dependencies: T4, T6, T7, T8, T9 (covers each as it lands; final pass after all).

**Deferred / not a v1 blocker (recorded so the rejection set is explicit):**

- **D1. `lint/rust` (clippy) auto-fix.** Needs a cargo-driven `clippy --fix` outside the bazel-aspect model and whole-crate rewrites; out of scope for changeset-scoped fixing. Effort: large.
- **D2. Per-built-in `suggested_fix` authoring** (e.g. `no-usfa-typo` corrected spelling). Incremental, per-check; framework support lands in T10. Effort: small each.
- **D3. Interactive / partial fixing** (per-hunk, per-finding selection). Explicit non-goal for v1. Effort: medium.
- **D4. W2 writable-sandbox WASM fix** as a first-class SDK option, pending the attentions decision. Effort: medium.
- **D5. Rename/create-file fixes** via copy-back. Unsupported by design in v1; revisit only if a real check needs it. Effort: medium.
- **D6. Declarative-runtime adoption of the sandbox for _checks_ (not just fix).** Orthogonal hardening, separate project. Effort: large.
