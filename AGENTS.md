- always use minimal bazel visibility, never default to public. Maintain bazel visibility health.
- Documentation-only changes (markdown files, design docs, plans, READMEs) should be pushed directly to `main` instead of opening a PR.
- Prefer `bazel test` / `bazel build` over `cargo test` / `cargo build`. Bazel's test cache reuses results across runs when sources are unchanged, which cargo cannot. For the engine specifically, run `bazel test //tools/boss/engine/...` instead of `cargo test -p boss-engine`. `cargo` is fine for quick local iteration on a single file, but PR-validation runs and "is this still passing?" sanity checks should go through bazel so the cache earns its keep.

## Hard constraint: fix failing checks at the root cause; never bypass them

When a CI check or repository check (checkleft, file-size, lint, test) is failing, fix the underlying problem. The following are forbidden bypasses — do NOT do any of them:

- Adding a file to a check exclusion or allowlist (`CHECKS.yaml` `exclude_files`, checkleft excludes, lint-disable comments, etc.) to suppress the failure.
- Setting `allow_bypass`, using an override flag, or invoking any bypass/override mechanism on a check.
- Passing `--no-verify` / skipping git hooks; adding broad `#[allow(...)]` / `// swiftlint:disable` / `# noqa` annotations solely to suppress a warning or error.
- Deleting, `#[ignore]`-ing, `xfail`-ing, skipping, or weakening assertions in a failing test to make it pass.
- Raising a threshold or limit (e.g. `max_lines` in a file-size check) solely to accommodate the offending file without reducing its size.

Required behavior: fix the real problem — split the oversized file, fix the lint/compile error, fix the test failure, resolve the root cause. If a check genuinely SHOULD be relaxed (a legitimately needed exclusion or threshold change), that is a human decision — STOP and surface it for operator approval with full justification. Do not decide this autonomously.

## Builder pattern convention

Structs with **8 or more fields** in `boss-protocol` (and in `boss-engine`'s internal types) use `#[derive(bon::Builder)]` with `#[builder(on(String, into))]`. This prevents additive-change PRs from touching every construction site across the repo.

Rules:
- `Option<T>` fields are automatically optional in the builder (bon defaults them to `None`).
- Non-optional fields that have a sensible runtime default (e.g. `autostart = true`, `priority = "medium"`, `last_status_actor = "human"`) carry `#[builder(default = ...)]`; use the existing `default_*()` helpers from `types.rs`.
- Fields with no sensible default remain required in the builder — omitting them is a compile error.
- When adding a new **optional** field to a builder-equipped struct: add `#[builder(default)]` (or `#[builder(default = expr)]`) alongside any `#[serde(default)]`. Existing construction sites need no changes.
- When adding a new **required** field: that is an explicit breaking change — call it out in the PR description. All construction sites must be updated.
- The production **DB mapper functions** (`map_task`, `map_product`, etc. in `work.rs`) continue to use struct literals — they must explicitly set every field from named columns, and a compile error when a new column isn't mapped is desirable. Do not convert DB mappers to builder calls.
- When calling `Option<String>` setter methods on a builder with `on(String, into)`: pass the inner string value directly (e.g. `.started_at("2026-01-01")`), **not** wrapped in `Some(...)`. To pass a dynamic `Option<&str>` or `Option<String>`, use the `maybe_field_name()` variant (e.g. `.maybe_repo_remote_url(repo)`).

Structs currently on the builder pattern: `Task`, `WorkExecution`, `Product`, `Project` (all in `boss-protocol/src/types.rs`).
