- always use minimal bazel visibility, never default to public. Maintain bazel visibility health.
- Documentation-only changes (markdown files, design docs, plans, READMEs) should be pushed directly to `main` instead of opening a PR.
- Prefer `bazel test` / `bazel build` over `cargo test` / `cargo build`. Bazel's test cache reuses results across runs when sources are unchanged, which cargo cannot. For the engine specifically, run `bazel test //tools/boss/engine/...` instead of `cargo test -p boss-engine`. `cargo` is fine for quick local iteration on a single file, but PR-validation runs and "is this still passing?" sanity checks should go through bazel so the cache earns its keep.

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
