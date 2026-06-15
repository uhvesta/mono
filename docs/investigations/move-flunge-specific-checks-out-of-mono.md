# `forbidden-patterns` and `require-companion-change`: decided design

Status: decided.
Date: 2026-06-14.

> **Implementation note (forbidden-patterns).** The mono-side change landed the generic check as a **wasm bundle** check under the `file/` namespace — `file/forbidden-patterns` — rather than a native rename. It ships in the single multiplexed preinstalled component alongside `file/size` and `file/ifchange`, following the same authorship pattern (an rlib check crate wired into `checkleft-preinstalled-bundle`). The native `forbidden-imports-deps` implementation was removed outright rather than kept as a deprecated alias (one implementation, not two); the flunge-side `CHECKS.yaml` migration to `check: file/forbidden-patterns` is a separate flunge change. The `rules`-array config surface in §1.1 is preserved exactly, so the flunge config below applies as written (only the `check:` id becomes `file/forbidden-patterns`). The `require-companion-change` half of this doc is unaffected and tracked separately.

> **Update (2026-06-15, implementation):** the `require-companion-change` half was
> implemented as a single generic WASM check `file/require-companion-change` that
> *subsumes* both the marker-based `ifchange-thenchange` / `file/ifchange` check and
> the glob-based `api-breaking-surface` check, rather than keeping them as two
> separate complementary primitives (§1.2's table below). The unified check supports
> both coupling-declaration mechanisms — in-source `LINT.IfChange`/`LINT.ThenChange`
> markers *and* config `trigger_globs`/`required_globs` — and exports `file/ifchange`
> and `api-breaking-surface` as deprecated aliases for the migration window. This
> consolidation (one implementation, not two) follows the T1775 rewrite of this
> decision. The "complementary, not substitutes" framing in §1.2 is superseded.

## Summary

Two checkleft built-in checks — `frontend-no-legacy-api` and `api-breaking-surface` — live in mono but are used only by flunge. Both are config-driven and generic in implementation; the flunge-specific parts are already entirely in flunge's `CHECKS.yaml`, not in the Rust code.

The approach: **generalize both checks into reusable checkleft primitives**, so generic mechanism stays in checkleft and flunge-specific policy stays in flunge.

- `frontend-no-legacy-api` → deleted from checkleft; re-expressed as flunge-side config of a renamed-and-generalized `forbidden-patterns` check (the existing `forbidden-imports-deps`, renamed for accuracy).
- `api-breaking-surface` → stays in checkleft, renamed `require-companion-change`; flunge keeps its globs and policy, only the referenced `check:` id changes.

This satisfies both goals at once: generic, reusable mechanisms live in checkleft (right for checkleft's generic-reuse principle); flunge-specific configuration lives in flunge (gets flunge concepts out of the shared binary).

---

## 1. The two generic checks

### 1.1 `forbidden-patterns`

A generic line-by-line regex scanner scoped to path globs. For each changed (non-deleted) file matching `include_globs`, every line is scanned; a finding is emitted per regex match. Each rule carries a `pattern`, `message`, `severity`, `remediation`, and `include_globs`/`exclude_globs` path filters.

This is the existing `forbidden-imports-deps` check (`tools/checkleft/src/checks/forbidden_imports_deps.rs`), renamed. The current name encodes a use case (import/dependency enforcement) rather than the mechanism; the implementation has no knowledge of import syntax and already matches any regex in any text file. `forbidden-patterns` accurately describes the mechanism and prevents future narrow one-off proliferation — new authors will find it rather than writing another single-purpose regex check.

**Config surface:**

```yaml
- id: no-legacy-api-fencingtracker          # policy id: drives findings, bypasses, severity
  check: forbidden-patterns
  config:
    rules:
      - pattern: '^\s*import\b[^;]*\bfrom\s+["''][^"'']*api/fencingtracker["'']'
        message: "import from deprecated frontend API module api/fencingtracker"
        severity: error
        include_globs: ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"]
        remediation: "Use supported frontend API modules under frontend/src/api/."
```

**Instance-per-policy model.** Each `- id:` entry in `CHECKS.yaml` is checkleft's policy unit — findings carry the policy `id:`, bypasses are keyed to it, severity is set at this level. For `forbidden-patterns`: **one check instance per logical prohibition.** Rules under a single `- id:` stanza are sub-clauses of the same prohibition (e.g., multiple deprecated module paths under one "no deprecated API imports" policy). Rules that represent distinct prohibitions — different owners, different bypass lifecycles, different remediation — each get their own `- id:` entry pointing at `check: forbidden-patterns`. (This matches the existing convention: `no-generated-artifacts` in mono's `CHECKS.yaml` groups related glob patterns under one `id:` because they are sub-clauses of the same "no build artifacts" policy.)

### 1.2 `require-companion-change`

A generic config-driven companion-change check: the policy-level counterpart to `ifchange-thenchange`. Over the changed-file set, it checks whether any file matches `trigger_globs` and whether any file matches `required_globs`. If a trigger file changed but no required file changed, one finding is emitted per trigger file.

This is the existing `api-breaking-surface` implementation (`tools/checkleft/src/checks/api_breaking_surface.rs`), renamed. The implementation is already completely generic — the Rust code contains zero domain-specific logic, no flunge concepts, no mention of APIs, backends, or fencing. Only the check's name and description encode flunge's domain. Renaming to `require-companion-change` makes it a legitimately reusable primitive for any organizational rule of the form "when surface X changes, companion Y must also change."

**Config surface:**

```yaml
- id: api-breaking-surface               # local policy label — flunge's choice, not the check mechanism
  check: require-companion-change
  policy: { allow_bypass: true }
  config:
    trigger_globs:
      - "backend/blob/src/app.rs"
      - "backend/blob/src/v2/mod.rs"
      - "backend/blob/src/v3/**"
    required_globs:
      - "docs/backend.md"
      - "docs/product-specs/**"
    message: "Potential backend API surface change without docs update."
    remediation: "When API behavior changes, update docs/backend.md or a relevant product spec in this PR."
```

**Relationship to `ifchange-thenchange`.** These are complementary primitives, not substitutes:

| | `ifchange-thenchange` | `require-companion-change` |
|---|---|---|
| Coupling declaration | `LINT.IfChange` annotations in source files | `CHECKS.yaml` config entry |
| Trigger granularity | Marked region within a specific file | Any file matching `trigger_globs` |
| New-file coverage | Manual — each new file must be annotated | Automatic — glob covers files not yet written |
| Policy location | Scattered across source files | One config entry |

`ifchange-thenchange` is _code-declared_ coupling, best for co-evolution contracts between specific code blocks a developer explicitly marks as coupled. `require-companion-change` is _policy-declared_ coupling, best for organizational rules like "API-surface changes must include a docs update" where the scope is defined by path globs that automatically cover future files. Neither replaces the other.

---

## 2. Flunge-side config

### 2.1 `frontend-no-legacy-api` → config of `forbidden-patterns`

The bundled `frontend-no-legacy-api` check is a narrow single-purpose re-implementation of what `forbidden-patterns` provides generically. It is deleted from checkleft. Flunge's policy is expressed as `forbidden-patterns` instances following the instance-per-policy model:

```yaml
- id: no-legacy-api-fencingtracker
  check: forbidden-patterns
  config:
    rules:
      - pattern: '^\s*import\b[^;]*\bfrom\s+["''][^"'']*api/fencingtracker["'']'
        message: "import from deprecated frontend API module api/fencingtracker"
        severity: error
        include_globs: ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"]
        remediation: "Use supported frontend API modules under frontend/src/api/."

- id: no-legacy-api-usafencing
  check: forbidden-patterns
  config:
    rules:
      - pattern: '^\s*import\b[^;]*\bfrom\s+["''][^"'']*api/usafencing["'']'
        message: "import from deprecated frontend API module api/usafencing"
        severity: error
        include_globs: ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"]
        remediation: "Use supported frontend API modules under frontend/src/api/."
```

Two separate instances because the two deprecated modules are distinct prohibitions that may sunset independently. If they are considered a single "no deprecated fencing API imports" policy with the same bypass lifecycle, they can be combined under one `id:` with a regex alternation `api/(fencingtracker|usafencing)`.

The pattern faithfully reproduces the original behavior: it matches ES import statements from any path ending in the legacy module name (covering `./api/fencingtracker`, `../../api/fencingtracker`, `@/api/fencingtracker`, etc.) in TypeScript/TSX files under `frontend/src/`. The `include_globs` reproduces the original `is_frontend_source_file` path filter.

### 2.2 `api-breaking-surface` → config of `require-companion-change`

No policy change required. Flunge's existing globs, message, remediation, and `allow_bypass` setting stay exactly as they are. The `id:` (flunge's local policy label) can remain `api-breaking-surface`. Only the `check:` field changes:

```yaml
- id: api-breaking-surface
  check: require-companion-change        # was: api-breaking-surface
  policy: { allow_bypass: true }
  config:
    trigger_globs: [ ... ]               # unchanged
    required_globs: [ ... ]              # unchanged
    message: "..."                       # unchanged
    remediation: "..."                   # unchanged
```

---

## 3. What changes in checkleft

### 3.1 `forbidden-imports-deps` → `forbidden-patterns`

- Register the check under the new id `forbidden-patterns`; keep `forbidden-imports-deps` as a deprecated alias for one release window.
- Update the description string and userdoc to describe the generic mechanism (line-by-line regex scanner over path globs, not import-specific).
- The instance-per-policy model is the specified config convention; userdoc examples should demonstrate it — one `- id:` per logical prohibition, not one stanza with unrelated policies bundled under a single `rules:` list.
- Delete `frontend-no-legacy-api`:
  - `src/checks/frontend_no_legacy_api.rs` — delete
  - `src/checks/mod.rs` — remove registration
  - `src/bypass.rs` — remove `BYPASS_FRONTEND_NO_LEGACY_API` and its references
  - `userdoc/docs/canned-checks.md` — remove entry
  - `docs/designs/bazel-repo-local-checks.md` and `userdoc/docs/checks-config.md` — update examples that use `frontend-no-legacy-api` as a running example; replace with a still-extant check
  - Test fixtures referencing the bundled check — update or remove

### 3.2 `api-breaking-surface` → `require-companion-change`

- Register the same `ConfiguredCheck` implementation under the new id `require-companion-change`; keep `api-breaking-surface` as a deprecated alias for one release window.
- Update the description string to the generic framing: "when files matching `trigger_globs` change, a companion change matching `required_globs` is required."
- Update userdoc to present this as a policy-declared companion-change check, complementary to `ifchange-thenchange`.

---

## 4. Migration path

### `forbidden-patterns` and `frontend-no-legacy-api`

1. **Mono/checkleft:** rename `forbidden-imports-deps` → `forbidden-patterns` (keep alias); delete `frontend-no-legacy-api` and its references. Cut a checkleft release.
2. **Flunge:** replace the `frontend-no-legacy-api` stanza in `CHECKS.yaml` with `forbidden-patterns` instances per §2.1. This is safe to land against either the old or new checkleft release — the binary knows both `forbidden-imports-deps` (aliased) and `forbidden-patterns`. Bump `bin/checkleft.lock` to the new release.
3. **Mono/checkleft:** remove the `forbidden-imports-deps` alias in a follow-on release once flunge has migrated.

### `require-companion-change` and `api-breaking-surface`

1. **Mono/checkleft:** register `require-companion-change` as the primary id; keep `api-breaking-surface` as a deprecated alias. Update description and userdoc. Cut a checkleft release.
2. **Flunge:** change `check: api-breaking-surface` → `check: require-companion-change` in `CHECKS.yaml`. Bump `bin/checkleft.lock`.
3. **Mono/checkleft:** remove the `api-breaking-surface` alias.

In both cases the check mechanism stays in checkleft; flunge owns only policy config.

---

## Alternatives considered

**Custom WASI/Component-Model check authored in flunge.** Flunge writes a Rust guest crate using checkleft's SDK, compiles it to a Component Model `.wasm`, and references it via `CHECKS.yaml`. Not viable today: the guest SDK (`checkleft-check-sdk`) is not published and depends on mono-internal path references; there is no non-Bazel build path; WIT versioning is not solved for external consumers. More importantly, flunge deliberately removed its prior external-check/build-coupled workflow and moved to a pinned-prebuilt model. These two checks need no custom logic that the generic primitives can't express, so this approach adds build and distribution overhead for no behavioral gain. Worth revisiting for an irreducibly specific check once the SDK is published and a cargo-based build path exists.

**Standalone CLI/script via the declarative runtime.** Checkleft's declarative runtime (shipped in the prebuilt binary) can spawn an external script and parse its findings — technically viable today with a path-bound checked-in script and zero build overhead. Not appropriate for these two checks because both generalize cleanly into config-driven built-ins; a script would re-implement in flunge logic checkleft already provides generically. The right trigger for this approach is a check whose logic genuinely cannot be expressed as a regex/glob/companion rule.
