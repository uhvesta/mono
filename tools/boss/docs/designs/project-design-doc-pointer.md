# Boss: Per-Project Design-Doc Pointer

## Problem

A Boss project (`projects` row) is the unit of work that owns a coherent
chunk of feature design plus its implementation tasks. The artefact that
*describes* a project — what it does, why, the resolved design questions —
is, in practice, a markdown file. In `mono` it lives at
`tools/boss/docs/designs/<slug>.md`; at work it would live in a separate
docs / wiki repo. Today, jumping from a project card in the kanban (or any
other project surface) to that doc is manual: the user has to remember
the convention, navigate the file tree, and trust that filename and
slug haven't drifted.

The recently-merged design-producing-tasks design
(`tools/boss/docs/designs/design-producing-tasks.md`) tackles a related
but distinct problem: it makes *the design task itself* a first-class
work-item kind, with the doc as its deliverable, and gives the engine a
`tasks.doc_ref` column for the row that *produced* the doc. That is
sufficient while the design task is the active deliverable — the user
opens the design *task* and the renderer shows the doc. It is *not*
sufficient once the design task is `done` and the project is humming
along on implementation tasks: the project card still wants a "go to my
design doc" affordance, and that affordance has to keep working long
after the design task has fallen out of the active kanban view. It also
has to work for projects whose design *did not* come from a Boss design
task at all — projects imported from work, projects whose doc was
hand-written before the design-task kind shipped, projects whose doc
lives in a docs-only repo Boss does not track as a Product.

This doc proposes a **per-project design-doc pointer**: a small,
structured field on `projects` that stably identifies "this project's
design doc lives at `<repo, branch, path>`," a resolution path that
handles same-repo and separate-repo cases, a UI affordance that opens
the doc with one click from any project surface, and clear coordination
rules with `design-producing-tasks` so the per-task `doc_ref` and the
per-project pointer never disagree.

## Goals

- A reliable, structured pointer on each `projects` row that identifies the project's design artefact.
- Resolution works across two environments: same repo as the product (the `mono` convention), and a separate doc repo that may not be a Boss-tracked Product.
- One-click "open design doc" affordance from the project card, project detail surface, and CLI.
- The pointer composes with the per-product `docs_location` (from `design-producing-tasks` Q3) and the per-task `doc_ref` (Q2) without ambiguity — exactly one source of truth at any given moment, with documented precedence.
- The pointer is settable manually (CLI / app), auto-derived on project create where that's safe, and auto-populated from a completing design task (when `design-producing-tasks` ships).
- A pointer that turns stale (file moved / renamed / deleted) is detected lazily and surfaced as a fixable error, not a silent broken link.
- Bootstrap path covers existing projects in `mono` whose design docs already exist on disk under the `tools/boss/docs/designs/<slug>.md` convention.

## Non-Goals

- **Editing or approving the design doc.** That lives entirely under `design-producing-tasks` (the `kind = 'design'` lifecycle, `DesignDetector`, `ApproveDesign`, manifest-driven materialisation). This project provides the pointer; it does not move bytes around or change row statuses.
- **Inline / threaded comments on the rendered doc.** Out of scope, covered by the doc-collab project (`proj_18a2bb78815be670_3`, archived).
- **Cross-repo dispatch.** Spawning workers in a repo that's not a Boss-tracked Product is the multi-repo project's concern (`proj_18a2bbe20fc03718_8`). This design only stores and resolves a *link* into such a repo — it does not need to lease a workspace there or track work-items there.
- **Multiple design docs per project.** One project, one design pointer. If a project legitimately splits its design across two docs, that's two projects (linked by a dependency edge), not one project with two pointers. This mirrors the one-PR-per-task and one-doc-per-design philosophy from `design-producing-tasks` Q1.
- **Watching the linked file for changes.** The pointer is data; we don't poll GitHub or `inotify` the workspace. Drift is detected lazily on click / fetch.
- **Reverse index ("which project owns this doc?").** A naive query is `SELECT * FROM projects WHERE design_doc_path = ?`, which is fine for the scale we're at; a reverse index can be added if it ever matters.

## Naming

- **`design_doc_ref`** — the pointer as a whole (the structured triple). Reuses the `doc_ref` noun from `design-producing-tasks` for consistency, prefixed with `design_` because `projects` may someday gain other kinds of artefact pointers (PRDs, retros) and we want the column name to be self-documenting.
- **The three components**: `design_doc_repo_remote_url`, `design_doc_branch`, `design_doc_path`. Same shape as `tasks.doc_ref` plus its `metadata.design.doc_branch` / `doc_repo_remote_url` companions.
- **CLI verb**: `boss project set-design-doc <selector> [--repo <url>] [--branch <b>] --path <p>` (and `--unset` to clear). Plus `boss project show <selector>` renders the resolved pointer in a single line.
- **Resolution result**: a `ResolvedDesignDoc` value carrying the absolute open-target — a `file://` URL into a leased workspace, an HTTPS GitHub URL, or "no pointer set."

---

## Design Question 1 — Where the Pointer Lives

### Options

- **(a) Three new columns on `projects`.** `design_doc_repo_remote_url TEXT NULL`, `design_doc_branch TEXT NULL`, `design_doc_path TEXT NULL`. The pointer is a first-class field on the row.
- **(b) One JSON column on `projects`.** `design_doc_ref TEXT NULL` carrying a serialised `{repo_remote_url, branch, path}`. Cheaper migration; harder SQL filtering.
- **(c) Reuse the auto-created design `task`'s `doc_ref` (from design-producing-tasks Q2).** Resolve at read-time by joining `projects → tasks WHERE kind = 'design' AND project_id = ?`.
- **(d) New table `project_artifacts` keyed by `(project_id, kind)`** where `kind = 'design_doc'` is one of several future artefact kinds.

### Discussion

(c) is conceptually appealing — the design task already produces the doc and stores the pointer in `tasks.doc_ref`, so why duplicate? Three reasons:

1. **Lifecycle mismatch.** A project may exist long after its design task is `done` and pruned from the active kanban. The user still wants to click the project card and see the design. Joining through `tasks` works, but the row that holds the pointer is then governed by the design task's lifecycle, not the project's. If the design task is deleted (rare, but legal), the project loses its pointer. That's a footgun.
2. **Cross-environment fit.** At work the design doc may *not* have been produced by a Boss design task — it lives in a wiki repo and was hand-authored, or imported. There's no `tasks` row of `kind = 'design'` for it. (c) requires conjuring a fake design task just to host the pointer. Bad shape.
3. **Project-create timing.** A project is often created with the design doc *already known* (the user is filing a project for design X they've already drafted). Storing the pointer on the project row at create time is the natural shape; (c) needs the engine to also create a placeholder design task whose only purpose is to carry the field.

(b) is an honest trade. JSON keeps the migration to one column. But the field is small, structured, and frequently filtered (`SELECT projects WHERE design_doc_repo_remote_url IS NULL` for "projects without a pointer"; `WHERE design_doc_path = ?` for the rare reverse lookup). Three text columns is nine bytes of denormalised redundancy and gains us native SQL filtering. Worth it.

(d) is overengineering for v1. The shape "one project, one design doc" is exactly 1:1, and a separate table buys flexibility we don't need. If we ever grow to "one project, many artefact kinds," we promote (a) to (d) with a one-shot migration.

(a) is the natural extension of how Boss treats first-class fields elsewhere (`products.repo_remote_url`, `tasks.pr_url`, `tasks.doc_ref` from design-producing-tasks). Three nullable text columns, no JSON, indexable, easy to filter.

### Recommendation

**Pick (a).**

```sql
ALTER TABLE projects ADD COLUMN design_doc_repo_remote_url TEXT;  -- NULL → inherit product.repo_remote_url
ALTER TABLE projects ADD COLUMN design_doc_branch          TEXT;  -- NULL → inherit product.docs_branch (or 'main')
ALTER TABLE projects ADD COLUMN design_doc_path            TEXT;  -- NULL → no pointer set
```

`design_doc_path` is the load-bearing field: when it is `NULL`, the project has no pointer and the UI affordance is hidden. When it is set, the other two are best-effort overrides.

Rendered on `Project` (mirrored in `Models.swift`):

```rust
pub struct Project {
    /* … existing fields … */
    pub design_doc_repo_remote_url: Option<String>,  // None → inherit
    pub design_doc_branch:          Option<String>,  // None → inherit
    pub design_doc_path:            Option<String>,  // None → no pointer
}
```

Three columns, not a struct, in the protocol type — consistent with how `Product` exposes `repo_remote_url` directly rather than a `RepoRef { url, branch }`. Resolution to a `ResolvedDesignDoc` happens engine-side (Q3).

#### Why not `Vec<DesignDocRef>` for forward-compat?

We considered modelling as a list (with `kind` discriminator) so future artefact pointers (PRD, retro, post-mortem) could share infrastructure. It is YAGNI for v1: there is no second kind in scope, the shape is harder to explain, and once we *do* want a second kind we already have the conversion path (table (d) above). The columns ship; if they get joined later, the migration is mechanical.

---

## Design Question 2 — Pointer Schema and Resolution

### The two real cases

- **In-repo (the `mono` default).** The doc lives in the same repo as the product the project belongs to. Storage: `design_doc_repo_remote_url IS NULL`, `design_doc_path = 'tools/boss/docs/designs/<slug>.md'`, `design_doc_branch IS NULL` (resolves to the product's default).
- **Separate-repo (the work case).** The doc lives in `https://github.com/myorg/wiki.git`. Storage: explicit `design_doc_repo_remote_url`, optional `design_doc_branch` (default `main`), `design_doc_path` is the path within that repo.

### Resolution rules

The engine offers `WorkDb::resolve_design_doc(project_id) -> Option<ResolvedDesignDoc>` whose contract is:

1. Read `(design_doc_repo_remote_url, design_doc_branch, design_doc_path)` off the project row.
2. If `design_doc_path` is `NULL` → return `None`. The project has no pointer; UI hides the affordance.
3. Otherwise, fill in defaults:
   - `repo := design_doc_repo_remote_url` ⊕ `product.repo_remote_url` ⊕ error (no repo to resolve against — surface as a fixable error, see Q5 failure modes).
   - `branch := design_doc_branch` ⊕ `product.docs_branch` ⊕ `"main"` (where `products.docs_branch` is a column added by `design-producing-tasks` Q3; if that design hasn't shipped yet, fall back directly to `"main"`).
   - `path := design_doc_path`.
4. Return `Some(ResolvedDesignDoc { repo, branch, path, kind })`, where `kind` is:
   - `Same(product_id)` if `repo == product.repo_remote_url`. The doc is in the project's own product's repo; a leased workspace likely contains it; the renderer / editor can read it from disk.
   - `External(repo)` otherwise. The doc is in a different repo. The doc may be in a Boss-tracked Product (look it up by `repo_remote_url` against `products`) or in an untracked external repo. Either way, the open affordance falls back to a GitHub web URL.

```rust
pub struct ResolvedDesignDoc {
    pub repo_remote_url: String,
    pub branch: String,
    pub path: String,
    pub kind: ResolvedDesignDocKind,
}

pub enum ResolvedDesignDocKind {
    /// Doc lives in the project's product's repo.
    SameProduct {
        product_id: String,
    },
    /// Doc lives in a Boss-tracked product different from the project's product.
    OtherProduct {
        product_id: String,
    },
    /// Doc lives in a repo Boss does not track.
    External,
}
```

#### Why structured `kind` rather than just a URL?

Because the open affordance behaves differently per kind. `SameProduct` gets first dibs at "open in editor on the leased workspace's filesystem" (no network). `OtherProduct` may have a leased workspace too — same fast path applies if so. `External` always falls back to web. The kanban renders a tooltip ("📄 design lives in this repo" vs "📄 design lives in `myorg/wiki`") that depends on the kind.

#### Error case: `design_doc_path` set but `design_doc_repo_remote_url` NULL and `product.repo_remote_url` NULL

Surface as an attention item on the project: *"Design doc pointer references a path but no repo can be resolved. Either set `design_doc_repo_remote_url` explicitly or set `repo_remote_url` on the product."* The pointer is "broken" until fixed.

### Recommendation

Three columns, the resolution rules above. `ResolvedDesignDoc` lives in `engine/src/work.rs` (or a sibling module if `work.rs` is already heavy); the protocol carries the raw three fields and the resolved struct on demand via a new `GetProjectDesignDoc` RPC (Q4).

---

## Design Question 3 — Open Affordance

### Where the affordance lives

- **Project card** on the kanban — primary surface. A small icon (📄 or "design") appears in the card's bottom-right when `design_doc_path` is set. Click opens the doc.
- **Project detail surface** (when one exists; today the kanban shows project rows in a header lane). A row "Design doc: `<resolved_path>` (open ↗)" with a click-to-open link.
- **CLI** — `boss project open-design <selector>` resolves the pointer and prints the open URL / path. With `--web` it forces the GitHub web URL even when a same-product workspace exists.

### What "open" does

The macOS app's open handler resolves the project's pointer, then dispatches by `ResolvedDesignDocKind`:

| Kind                  | Workspace available? | Open target                                                  |
|-----------------------|---------------------|--------------------------------------------------------------|
| `SameProduct`         | yes                 | renderer window (when shipped) **or** `$EDITOR` on the file in the leased workspace |
| `SameProduct`         | no                  | GitHub web URL (`https://github.com/<owner>/<repo>/blob/<branch>/<path>`) |
| `OtherProduct`        | yes (cube has it)   | same as `SameProduct` "yes"                                   |
| `OtherProduct`        | no                  | GitHub web URL                                                |
| `External`            | n/a                 | GitHub web URL                                                |

"Workspace available" means cube has at least one workspace leased for the relevant `repo_remote_url`. The macOS app can ask the engine via the existing cube-state RPC; the engine threads through.

For the renderer, this design coordinates with `design-producing-tasks` Q6 (`DesignRendererView`): if that view has shipped, the project's open affordance reuses it (the renderer takes a `ResolvedDesignDoc` rather than only a `task_id`). If it has not yet shipped, fall back to `$EDITOR` / web. The two designs ship independently; the project pointer doesn't *require* the renderer.

### CLI form

```
boss project open-design <selector>          # prints / opens the resolved target
boss project open-design <selector> --web    # forces web URL
boss project open-design <selector> --print  # prints the URL/path, doesn't open
```

The macOS app calls a new RPC `OpenProjectDesignDoc { project_id }` whose output is `ResolvedDesignDoc | NotSet | BrokenPointer { reason }` so the app can render the right affordance state without round-tripping the resolution logic itself.

### Recommendation

**Project card icon + CLI verb is the v1 surface.** A formal "project detail" surface is out of scope (the app doesn't have one yet); when it lands, it picks up the same affordance.

The icon is hidden when `design_doc_path` is `NULL`, badged with a warning glyph when the pointer resolves to `BrokenPointer` (Q5), and a plain "📄" otherwise. The hover tooltip renders `<repo_basename>:<path>` so the user knows where they're going before clicking.

---

## Design Question 4 — Wire and RPC Shape

### Current `Project` projection

```rust
pub struct Project {
    pub id: String,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub priority: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_status_actor: String,
}
```

### v1 additions

```rust
pub struct Project {
    /* … existing fields … */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,
}
```

`#[serde(default)]` on each so older clients deserialising newer projects keep working; older payloads without the fields stay valid because they default to `None`.

### Mutation RPCs

```rust
// types.rs
pub struct SetProjectDesignDocInput {
    pub project_id: String,
    /// `None` means "inherit from product.repo_remote_url" (in-repo case).
    pub design_doc_repo_remote_url: Option<String>,
    /// `None` means "inherit from product.docs_branch (or 'main')".
    pub design_doc_branch: Option<String>,
    /// `None` clears the pointer entirely. Setting `Some("")` is rejected.
    pub design_doc_path: Option<String>,
    /// If true, treat this as a clear (path = NULL).
    #[serde(default)]
    pub unset: bool,
}

pub struct ResolveProjectDesignDocOutput {
    pub project_id: String,
    pub state: ProjectDesignDocState,
}

pub enum ProjectDesignDocState {
    NotSet,
    Resolved {
        resolved: ResolvedDesignDoc,
        /// True when a leased cube workspace exists for `resolved.repo_remote_url`.
        local_workspace_available: bool,
        /// `https://github.com/<owner>/<repo>/blob/<branch>/<path>` for the
        /// kanban tooltip / right-click "copy link."
        web_url: String,
    },
    Broken { reason: String },
}

// wire.rs
SetProjectDesignDoc      { request_id: String, input: SetProjectDesignDocInput }
ResolveProjectDesignDoc  { request_id: String, project_id: String }
```

`ResolveProjectDesignDoc` is read-only and cheap (no network) — the kanban can call it lazily per project as cards come into view, or batch via a future `ResolveProjectDesignDocs(Vec<project_id>)` if the per-project chatter is too noisy. v1 ships the singular form.

### Bulk read

`WorkTree` (returned from `Subscribe`) is the kanban's main data feed. `Project`s already serialise into it; the new three columns ride along automatically. The kanban still needs to know *resolved* state (specifically `local_workspace_available`), but that depends on cube's state which can change independently. v1 has the kanban fetch resolution per-project on render; if it becomes a hot path we add a `WorkTree.project_design_doc_states` companion.

### CLI

```
boss project set-design-doc <selector> --path <p> [--repo <url>] [--branch <b>]
boss project set-design-doc <selector> --unset
boss project show <selector>           # pre-existing; gains a "Design doc: …" line
boss project open-design <selector> [--web] [--print]
```

`<selector>` is the standard project selector (id or `<product>/<project_slug>`).

### Recommendation

Three columns on the wire `Project`; one `Set…` RPC for writes; one `Resolve…` RPC for reads. CLI verbs match.

---

## Design Question 5 — Failure / Drift Handling

### Cases

- **Pointer set; file moved or renamed on disk.** Click opens; resolver returns `Resolved`; the renderer / editor 404s on read. The renderer surfaces "file not found at `<path>`" with a "Re-point …" button that opens the set-design-doc form pre-populated with the current values.
- **Pointer set; doc deleted entirely.** Same as moved — broken on click. Resolver still returns `Resolved` because we don't probe; the broken state is detected at the open step.
- **Pointer set; path traversal or absolute path.** Reject at write time. `design_doc_path` must be relative and must not contain `..` segments. Surface a `CliError::Validation`.
- **Pointer set; repo URL points to a private repo the user can't access.** Resolver doesn't probe. Web URL fallback works (auth happens in the browser); editor fallback works (the leased workspace is already authenticated). Same-product case is unaffected.
- **Same-product pointer; same-product workspace not leased.** Resolver returns `Resolved` with `local_workspace_available = false`; the open affordance falls back to the GitHub web URL. Same code path as `External`.
- **Project deleted.** SQL `ON DELETE CASCADE` semantics aren't enabled here (Boss uses soft deletes / status flips elsewhere). When a project is deleted, the columns go with the row. No orphan pointers.
- **Pointer set explicitly to a path that doesn't exist (yet).** This is *legal* — the user is filing a project ahead of the design doc's existence. The "broken on click" UX is acceptable: the user knows they haven't written the doc yet. We don't validate existence on `SetProjectDesignDoc` because that requires either a network call (separate-repo case) or a workspace lease (same-repo case), neither of which the engine should block on.

### Probe strategy

**Validate cheaply at write; fail visibly at open.** Specifically:

- Write-time validation: relative path, no `..`, not blank, repo URL parses as `https://…` or `git@…` if set. Path must end in `.md`. (Future-proofing: allow `.markdown` too. Reject `.html`, `.txt`, `.pdf` for v1 — design docs are markdown.)
- Open-time validation: read the file (filesystem or `gh api`); if it 404s, surface a fixable error inline. Do not flip the project's status to `blocked` or anything — broken pointers are advisory, not blocking.

### Stale-on-rename

A common case in this repo is renaming `tools/boss/docs/designs/foo.md` to `tools/boss/docs/designs/bar.md`. The pointer doesn't auto-update. Three mitigations:

1. **`boss project set-design-doc <selector> --path <new_path>`** is one CLI invocation.
2. **A lint chore** could scan all projects and report broken pointers (`boss project lint-design-docs`). v1 doesn't ship the lint; it's a follow-up.
3. **A future "watch the workspace for rename events"** hook is explicitly out of scope — it's complex and the user can fix manually faster than it takes to debug rename-detection.

### Recommendation

Cheap write-time validation, lazy open-time detection, no auto-repair. The lint chore is a follow-up.

---

## Design Question 6 — Coordination with `design-producing-tasks`

### Where the truth lives

- **`tasks.doc_ref`** (and `tasks.metadata.design.*`) records *the doc that this `kind = 'design'` task produced*. It is task-local: it survives only as long as the task row, and updates as the task iterates (revise → new sha → new in_review).
- **`projects.design_doc_path`** (and the two siblings) records *the project's pointer to its design doc*. It is project-local and survives long after the design task is `done`.

These are not the same field — they have different lifecycles, different write paths, and answer different questions ("what did this design task produce?" vs "where does this project's design live?"). They *should* agree at any moment in time when both are populated.

### Sync rules

The engine performs three sync operations:

1. **On `DesignDetector` firing** (the design task moves to `in_review` and stamps `tasks.doc_ref`): copy `(repo_remote_url, branch, file_path)` from the task into the parent project's `design_doc_*` columns **iff** the project's `design_doc_path` is currently `NULL`. The user-set value wins; the auto-populate only fills empty pointers. Reasoning: a user who has already pointed the project at a hand-authored doc should not have it silently overwritten when the design task lands; but a user who has not set anything benefits from auto-population.

2. **On `ApproveDesign`** (the design task moves to `done`): re-affirm the project's pointer matches the approved doc's location. If the project's pointer differs (the user manually set it to something else after auto-population), surface a `WorkAttentionItem`: *"Project pointer (`<project_path>`) and approved design doc (`<task_path>`) differ. Update the project pointer or revoke the approval."* Don't auto-overwrite.

3. **On `boss project set-design-doc`** with values that disagree with an in-flight design task's `doc_ref`: warn but allow. Confirmation prompt in interactive CLI; non-interactive callers proceed with the override.

### What if `design-producing-tasks` hasn't shipped yet?

This design ships independently. If `design-producing-tasks` is unbuilt, the auto-create design task on `create_project` is still a `kind = 'design'` row (the `insert_design_task_for_project_in_tx` already emits it), but the row never reaches `in_review` via `DesignDetector` because that detector doesn't exist yet. The project's `design_doc_*` columns are then *only* populated via manual CLI / app, which is exactly the same fallback the work environment needs anyway.

When `design-producing-tasks` lands, sync rule (1) starts firing automatically and historical projects with manually-set pointers are unaffected.

### Symmetric direction (project → task)

Setting `design_doc_path` on a project does **not** populate the design task's `doc_ref`. The design task's `doc_ref` is *the artefact the worker produced*, not a hint at the future location. If the project pointer is set first (because the user is filing a project ahead of design work), the design task spawn prompt can still *read* the project's pointer (Q3 of `design-producing-tasks` already feeds `docs_location` / `slug` into the prompt; we extend it to fall back to the project's pointer when set). But the engine does not write `tasks.doc_ref` until the `DesignDetector` fires.

### Recommendation

**One-way sync (task → project) on detection, with conflict surfacing on approval.** The project pointer is the long-lived field; the task pointer is the short-lived deliverable record. They sync only when there is no conflict; conflicts go to attention items, not auto-overwrites.

---

## Design Question 7 — Bootstrap

### Existing projects in `mono`

The current state of `tools/boss/docs/designs/` (already-merged docs) covers most of the existing projects in `mono`'s Boss DB. A bootstrap chore can:

1. Enumerate every `projects` row whose `design_doc_path` is `NULL`.
2. For each row, look for `tools/boss/docs/designs/<project.slug>.md` in the product's repo (the workspace has a checkout, since this is `mono`).
3. If the file exists, set `design_doc_path = 'tools/boss/docs/designs/<project.slug>.md'`. Leave `design_doc_repo_remote_url` and `design_doc_branch` `NULL` (inherit).
4. If no file matches, leave the project with `design_doc_path = NULL` and the affordance hidden. The user fills in manually.

This is a one-shot script run by hand (`boss admin backfill-project-design-docs`) rather than a SQL migration, because the slug→file matching wants to *report* what it did and skip non-matches. Ship it as a CLI verb, not a schema migration.

For the work environment, no bootstrap is run — the user manually points each project as they land.

### Existing design docs without a project

Some `tools/boss/docs/designs/*.md` files describe *chores* (this very repo's `auto-rebase-stacked-prs.md` was a chore, not a project). Those don't need a project pointer. The bootstrap script ignores files that don't match any project's slug.

### Recommendation

One-shot `boss admin backfill-project-design-docs` verb; manual fill for the rest.

---

## Design Question 8 — Same-Repo Path Format

### Options

- **(α) Repository-relative path.** `design_doc_path = 'tools/boss/docs/designs/foo.md'`. Always relative to the resolved `repo_remote_url`'s root.
- **(β) Workspace-absolute path.** `/Users/.../tools/boss/docs/designs/foo.md`. Fragile across machines.
- **(γ) Path-fragment URL.** `https://github.com/owner/repo/blob/main/tools/boss/docs/designs/foo.md` — full URL stored.
- **(δ) Tagged union.** Either a relative path (in-repo case) or a full HTTPS URL (separate-repo case), with the column figuring out which by prefix.

### Discussion

(β) is dead on arrival — workspace paths change between machines and lease cycles.

(γ) sounds tidy (one column!) but loses information: the GitHub blob URL is a *rendering* URL, not a source-of-truth. `gh api /contents/...` wants `(owner, repo, path, ref)` and we'd have to *parse* the blob URL back to those four fields, with all the URL-decoding fun that entails. Plus, `https://github.com/foo/bar/blob/refs/heads/main/...` and `https://github.com/foo/bar/blob/main/...` and `https://github.com/foo/bar/tree/main/...` are all different valid forms. Storing structured triples and *rendering* the URL on demand is the right shape.

(δ) is a stealth (γ): same parsing problem on read.

(α) wins. The repo URL is in `design_doc_repo_remote_url` (or inherited from the product); the branch is in `design_doc_branch` (or inherited); the path is repo-relative. Three fields, no URL parsing, web URL is rendered with one `format!`.

### Recommendation

**Always store `design_doc_path` as a repo-relative path** with a leading non-slash character. Reject leading `/`, leading `.`, embedded `..`, and absolute paths at write time.

```rust
fn validate_design_doc_path(p: &str) -> Result<()> {
    if p.is_empty() { bail!("design_doc_path may not be empty (use --unset to clear)"); }
    if p.starts_with('/') { bail!("design_doc_path must be repo-relative (no leading `/`)"); }
    if p.split('/').any(|seg| seg == "..") { bail!("design_doc_path may not contain `..`"); }
    if !p.ends_with(".md") && !p.ends_with(".markdown") { bail!("design_doc_path must reference a markdown file"); }
    Ok(())
}
```

The `--repo <url>` CLI flag accepts any GitHub URL form (`https://…/repo`, `https://…/repo.git`, `git@github.com:owner/repo.git`); the engine canonicalises to the user's preferred form (`https://…git`) at write time so `repo_remote_url` matches the convention used elsewhere in the schema.

---

## Design Question 9 — Renderer Reuse

### Context

`design-producing-tasks` Q6 specifies a `DesignRendererView` SwiftUI window that takes a `task_id`, calls a `GetDesignDoc(task_id)` RPC, and renders Textual markdown. v1 of *that* design ties the renderer to a design *task*.

This project's open-affordance for `SameProduct` (workspace available) wants to render the *project's* design doc. Three options:

- **(M) Reuse the renderer with a `ResolvedDesignDoc` constructor.** Generalise the renderer's input from `task_id` to `(task_id | resolved_doc)`. Add a second RPC `GetDesignDocFromRef(repo, branch, path)` for the project case.
- **(N) Open in `$EDITOR` only — the renderer is design-task-specific.** Cleaner separation; uglier UX (no in-app preview).
- **(O) Always GitHub web URL for the project surface.** Consistent across kinds; gives up on the Textual renderer for projects.

### Discussion

(N) is the cleanest separation but kills the "click the project card and read the doc in-app" UX. Friction.

(O) is consistent but wastes Textual once `design-producing-tasks` builds it. The renderer is the right surface for design docs; it should serve all design-doc surfaces, not just the design-task one.

(M) is the right end state and a small extension to `design-producing-tasks`. The renderer's input becomes "give me this doc," parameterised by either `task_id` (which the engine resolves to `tasks.doc_ref`) or `(repo, branch, path)` directly. The Approve button is hidden when the input is the latter form (no task to approve).

### Recommendation

**Pick (M).** Coordinate with `design-producing-tasks` Q6 to make `DesignRendererView`'s init take an enum input:

```swift
enum DesignRendererSource {
    case designTask(taskId: String)   // shows Approve button etc.
    case projectPointer(projectId: String, resolved: ResolvedDesignDoc)  // read-only
}
```

The engine RPC for the second form is `GetProjectDesignDoc(project_id) -> { content, web_url }` (a thin wrapper over `ResolveProjectDesignDoc` + filesystem / `gh api` read). `design-producing-tasks` ships v1 with `case designTask` only; this project lands `case projectPointer` as a follow-up after the renderer exists. Until both ship, the project surface uses `$EDITOR` / web fallback.

---

## Design Question 10 — Permissions and Authorship

### What gets recorded on `SetProjectDesignDoc`

The `projects.last_status_actor` column is currently used to gate auto-block / unblock decisions; this design does **not** flip status, so we don't touch `last_status_actor`. Setting the pointer is a *property edit*, not a status transition. We do bump `updated_at`.

### Audit trail

The engine doesn't currently keep an audit log of project-property edits (`description`, `goal`, `priority` etc. are all overwritten in place). Adding an audit log is a separate concern; the pointer rides along with the existing convention.

### Multi-user

Single-user repo today. No authz on `SetProjectDesignDoc` beyond the standard caller identity (`'human'`). Future multi-user setups can layer authz the same way they will for every other property edit.

### Recommendation

**No special permissions; standard property-edit semantics.** `updated_at` bumps on write; `last_status_actor` stays untouched.

---

## Design Question 11 — Relationship to Other Open Work

### `design-producing-tasks`

Covered by Q6. One-way sync, no overwrites, conflicts surfaced as attention items.

### `proj_18a2bbe20fc03718_8` (Multi-repo / cross-product modelling)

Overlap is real: a separate-repo design doc points into a repo Boss may or may not track as a Product. This design does **not** require Boss to track the doc's repo as a Product — the pointer stores the bare `repo_remote_url` and the open affordance gracefully degrades to "GitHub web URL." When multi-repo modelling lands, projects whose pointer references a Boss-tracked-but-different Product get fancier behaviour (workspace-resolved open) but the schema doesn't change.

We coordinate: the multi-repo project's design (when written) should treat the per-project pointer as a *consumer* of cross-product workspace lookup, not a source. The pointer's job is to identify *where* the doc is; the multi-repo project's job is to make *getting to* that location reliable when "where" spans repos.

### `boss task bind-pr` thread

Irrelevant — this design is about a project pointing at a doc, not a task pointing at a PR.

### One-deliverable-per-row philosophy

Reaffirmed at the project level: one project, one design doc. If a project legitimately has two docs, that's a smell — split into two projects, link with a dependency edge.

### What becomes moot

- **Manual filesystem hunts for "where's the design doc for project X."** Replaced by `boss project open-design <selector>`.
- **Implicit slug→filename conventions** as the only way to find docs. The convention still holds for `mono` defaults, but the pointer is canonical.

---

## Design Question 12 — Failure Modes

### `design_doc_path` set, `design_doc_repo_remote_url` NULL, `product.repo_remote_url` NULL

Covered in Q2: surface as `BrokenPointer` on resolve. UI shows the warning glyph; CLI errors with a clear message naming both columns.

### Two projects pointing at the same doc

Legal but unusual. No uniqueness constraint on `(product_id, design_doc_path)`. If a user does this, both projects get an open affordance to the same doc. Probably a sign the projects should be merged; we don't enforce it.

### Pointer set; repo gone (org renamed, repo deleted)

`gh api` 404s; the renderer surfaces the underlying error. The user updates the pointer with `boss project set-design-doc --repo <new_url>`.

### Concurrent `SetProjectDesignDoc` from two clients

SQL serialises; last-writer-wins. No conflict handling; the property-edit semantics elsewhere in Boss are the same. If two users routinely race we add a `If-Match: <updated_at>` shape, but v1 trusts SQLite-level ordering.

### Manifest-driven project creation (from `design-producing-tasks` Q8)

When the engine applies a design manifest, it creates new `projects` rows. Those rows get auto-populated `design_doc_*` columns referencing the *parent design doc* — i.e. the doc that *spawned them*. (Not their *own* design docs, which don't exist yet.) Hmm — that's wrong. Each spawned project's design doc is *its own* future doc, not the parent's.

Resolution: the manifest's `projects` entries should NOT auto-populate the spawned project's `design_doc_*`. Spawned projects start with `NULL` pointers and the user fills them in once those projects' design tasks produce docs (or the user hand-points them). The parent design's doc is referenced by the *parent project*, not the children.

Sync rule (1) from Q6 then fires for each spawned project independently when its own design task moves to `in_review`. Clean recursion.

### Migration of existing `tasks.kind = 'design'` rows

`insert_design_task_for_project_in_tx` already emits a placeholder design task per project. Those tasks have `doc_ref = NULL` today. This design's migration adds the three columns to `projects` with no automatic backfill; the bootstrap chore (Q7) handles backfill for `mono`. Fresh installs start with NULL pointers everywhere. No changes needed to existing design tasks.

### The pointer's repo URL canonicalisation

`https://github.com/foo/bar` and `https://github.com/foo/bar.git` and `git@github.com:foo/bar.git` all refer to the same repo. We canonicalise on write to match `products.repo_remote_url`'s form (whatever the existing canonicaliser uses; reuse it). Reads return the canonical form; web-URL rendering parses it.

### Recommendation

Lazy detection, simple property-edit semantics, no auto-repair, attention items for ambiguous cases.

---

## Schema and Wire Summary

### Column adds

```sql
ALTER TABLE projects ADD COLUMN design_doc_repo_remote_url TEXT;  -- NULL → inherit product.repo_remote_url
ALTER TABLE projects ADD COLUMN design_doc_branch          TEXT;  -- NULL → inherit product.docs_branch (or 'main')
ALTER TABLE projects ADD COLUMN design_doc_path            TEXT;  -- NULL → no pointer (UI hides affordance)
```

Three nullable text columns. No CHECK constraints; validation lives in the application layer (Q8). `tasks.metadata.schema_version` bumps if the schema-version convention from `design-producing-tasks` is in place; otherwise no bump.

### Protocol additions

```rust
// types.rs
pub struct Project {
    /* … existing fields … */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,
}

pub struct SetProjectDesignDocInput {
    pub project_id: String,
    pub design_doc_repo_remote_url: Option<String>,
    pub design_doc_branch: Option<String>,
    pub design_doc_path: Option<String>,
    #[serde(default)]
    pub unset: bool,
}

pub struct ResolvedDesignDoc {
    pub repo_remote_url: String,
    pub branch: String,
    pub path: String,
    pub kind: ResolvedDesignDocKind,
}

pub enum ResolvedDesignDocKind {
    SameProduct  { product_id: String },
    OtherProduct { product_id: String },
    External,
}

pub enum ProjectDesignDocState {
    NotSet,
    Resolved {
        resolved: ResolvedDesignDoc,
        local_workspace_available: bool,
        web_url: String,
    },
    Broken { reason: String },
}

pub struct ResolveProjectDesignDocOutput {
    pub project_id: String,
    pub state: ProjectDesignDocState,
}

// wire.rs
SetProjectDesignDoc      { request_id: String, input: SetProjectDesignDocInput }
ResolveProjectDesignDoc  { request_id: String, project_id: String }
```

CLI:

```
boss project set-design-doc <selector> --path <p> [--repo <url>] [--branch <b>]
boss project set-design-doc <selector> --unset
boss project show <selector>           # gains a "Design doc:" line
boss project open-design <selector> [--web] [--print]
boss admin backfill-project-design-docs   # one-shot, mono only
```

### Engine module split

- `engine/src/work.rs` — add `WorkDb::set_project_design_doc`, `WorkDb::resolve_project_design_doc` (the resolver; consults `products.repo_remote_url` and `products.docs_branch`), plus the column read in `map_project`.
- `engine/src/rpc.rs` (or wherever RPC dispatch lives) — handle `SetProjectDesignDoc` and `ResolveProjectDesignDoc`.
- `cli/src/commands/project.rs` — add `set-design-doc` / `open-design` / `--show-design-doc` flag.
- `cli/src/commands/admin.rs` — `backfill-project-design-docs`.
- `Models.swift` — mirror `Project`'s three new fields, `ResolvedDesignDoc`, `ProjectDesignDocState`.
- `ContentView.swift` — kanban project-card affordance: hidden when `NotSet`, plain icon when `Resolved`, warning glyph when `Broken`. Click → `ResolveProjectDesignDoc` → dispatch by kind.
- `DesignsView.swift` — optional: when a project is selected in the existing designs browser, scroll to / highlight the project's pointed-at file.

### Topic / event

Reuse `work_item_changed` for `Project` rows when their pointer is set/cleared (an existing event the kanban subscribes to). No new topics in v1.

---

## Sequence Diagram — Set Pointer + Open

```
┌────────┐  ┌───────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐
│ user   │  │ kanban /  │  │ engine   │  │ workspace│  │ sqlite  │
│ (CLI/  │  │ macOS app │  │          │  │ / gh api │  │         │
│  app)  │  │           │  │          │  │          │  │         │
└───┬────┘  └─────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬────┘
    │ SetProjectDesignDoc       │             │             │
    │  (path='tools/.../foo.md')│             │             │
    │ ──────────────────────────────────────► │             │
    │                            │ validate path           │
    │                            │ UPDATE projects        │
    │                            │ ──────────────────────────────────►│
    │                            │ work_item_changed event             │
    │ ◄──────────────────────── │             │             │
    │ kanban refreshes; project card now shows 📄 icon      │
    │ ◄──────────────────────── │             │             │
    │ user clicks 📄            │             │             │
    │ ────────────────────────► │             │             │
    │                            │ ResolveProjectDesignDoc │
    │                            │ ◄────────── │             │
    │                            │ resolve repo, branch, path                  │
    │                            │ check cube workspace lease state           │
    │                            │ → state = Resolved { same_product, local=t}│
    │                            │ ─────────► │             │
    │ app picks SameProduct path; opens renderer (or $EDITOR fallback)         │
    │                            │            │ read file from workspace fs   │
    │                            │            │ ─────────► │             │
    │                            │            │ ◄───────── │             │
    │ user reads doc            │             │             │             │
```

---

## Risks

**R1 — Two truths (project pointer vs design task `doc_ref`).** The pointer on `projects` and the `doc_ref` on the auto-created design task can disagree. Mitigation: Q6 sync rules; conflicts surface as attention items, not silent overwrites. The user always wins.

**R2 — `design-producing-tasks` ships later or differently than designed.** This design depends on `design-producing-tasks` Q3 (`products.docs_location` / `docs_branch`) for branch defaults and Q6 (renderer) for in-app preview. Mitigation: graceful degradation — branch defaults to `main` if `products.docs_branch` doesn't exist; renderer reuse becomes a no-op until the renderer exists. The project pointer ships independently.

**R3 — Bootstrap mis-matches.** The `mono` backfill (Q7) matches by slug → filename. If a project's slug doesn't match its doc's filename (we have at least a few in flight: `auto-rebase-stacked-prs`, `work-dependencies` etc., where slug equals filename, so most work), the backfill misses. Mitigation: backfill prints what it set and what it skipped; user fills the rest manually.

**R4 — Stale pointers across renames.** Renaming a doc on disk leaves the pointer stale. Mitigation: open-time error message names the old path so the user can fix it; a `boss project lint-design-docs` follow-up could batch-detect.

**R5 — Path validation false positives.** The validator rejects absolute paths, `..`, non-`.md` extensions. If a user genuinely has a design doc at `/Users/.../foo.md` in a repo (impossible, but) or wants `.rst`, they're stuck. Mitigation: the rules cover every real case; if a user hits an exception we relax the validator with a follow-up.

**R6 — External-repo pointer pointing at a private repo without auth.** Web URL fallback means GitHub asks the browser to log in; that works. Editor fallback is impossible (no checkout). Mitigation: this is the same constraint as any external link — no special handling needed. Document in the open-affordance UX.

**R7 — Resolver is read-heavy.** Every project card calls `ResolveProjectDesignDoc` on render. For 100 projects that's 100 RPC calls. Mitigation: ship the singular RPC v1; if profiling shows hot path, add `ResolveProjectDesignDocs(Vec<id>)` and batch.

**R8 — Manifest-spawned projects' pointers are NULL.** Q12: spawned projects don't auto-populate from the parent design's manifest. The user has to set them manually as those projects produce their own design docs. Mitigation: this is correct behaviour; document it in the manifest schema (the manifest's `projects` entries should NOT carry a `design_doc_path` field, because that field is *that project's own* doc, not yet existing).

**R9 — Coordination with cross-product (`proj_18a2bbe20fc03718_8`).** When the doc lives in another Boss-tracked product's repo, the resolver returns `OtherProduct` and the open affordance falls back to web URL until cross-product is built. Mitigation: documented in Q3; cross-product project picks up the local-open path as a consumer.

**R10 — Schema additions blocked by an in-flight `design-producing-tasks` migration.** Both designs add columns; they touch different tables (`projects` vs `tasks` / `products`) so they don't conflict. Mitigation: the migrations are commutative; either ships first.

---

## Follow-up Implementation Chores (to enqueue once approved)

Bite-sized; each fits one worker session.

1. **Schema + migration**: add `projects.design_doc_repo_remote_url`, `design_doc_branch`, `design_doc_path` columns. Idempotent; existing rows default to `NULL`. Acceptance: fresh init and migration both yield the new schema; `Project` queries return the new fields; existing tests pass.

2. **Protocol types**: extend `Project`, add `SetProjectDesignDocInput`, `ResolvedDesignDoc`, `ResolvedDesignDocKind`, `ProjectDesignDocState`, `ResolveProjectDesignDocOutput`. Mirror in `Models.swift`. Acceptance: serde / Codable round-trips green; existing wire tests still pass.

3. **Engine: `set_project_design_doc`** — `WorkDb` method with path validation (Q8), repo URL canonicalisation, `unset` handling. Acceptance: unit tests cover path validation cases (empty, absolute, `..`, bad extension), unset path, repo canonicalisation, last-writer-wins.

4. **Engine: `resolve_project_design_doc`** — read columns, fall back to product, build `ResolvedDesignDoc`, classify kind, render `web_url`, check cube lease state. Acceptance: unit tests cover same-product / other-product / external cases; broken pointer surfaces with a clear reason.

5. **Engine: `SetProjectDesignDoc` + `ResolveProjectDesignDoc` RPCs**. Acceptance: integration test: CLI sets a pointer, kanban resolves it, both shapes match.

6. **Engine: sync rule on `DesignDetector`** — when the design task moves to `in_review`, copy `(repo, branch, path)` into the parent project's columns iff currently `NULL`. Acceptance: integration test creates a design task, fakes a `DOC_REF` stop, asserts the project's pointer is populated; with a manually-set pointer, asserts no overwrite.

7. **Engine: conflict surfacing on `ApproveDesign`** — if the project's pointer differs from the approved doc's location, emit a `WorkAttentionItem`. Acceptance: integration test for the conflict path.

8. **CLI: `boss project set-design-doc | open-design`** — full noun additions; `boss project show` gets a "Design doc:" line. Acceptance: `--help` covers each verb; integration test covers set → resolve → open.

9. **CLI: `boss admin backfill-project-design-docs`** — one-shot script for `mono`. Acceptance: dry-run mode reports what would change; real run fills pointers and prints a summary.

10. **macOS: project-card affordance** — icon variant per `ProjectDesignDocState`; click → `ResolveProjectDesignDoc` → dispatch by kind. Acceptance: snapshot tests for each state; click handler covered by UI test.

11. **macOS: open dispatch** — same-product + workspace-available → `$EDITOR` (until renderer exists), otherwise web URL. Acceptance: manual test in both environments.

12. **(After `design-producing-tasks` Q6 ships) Renderer reuse** — extend `DesignRendererView` to accept `case projectPointer`; project surface uses the renderer for `SameProduct` opens. Acceptance: Approve button hidden in project-pointer mode; doc renders identically.

13. **(Optional follow-up) `boss project lint-design-docs`** — batch-detect broken pointers, print a fixable list. Out of v1.

14. **(Optional follow-up) Batch resolver** — `ResolveProjectDesignDocs(Vec<id>)` if profiling shows the per-project RPC is hot.

15. **(Optional follow-up) Audit log on property edits** — track who set the pointer when. Cross-cutting; not gated by this design.

---

## Out of Scope

- Editing or approving the design doc (covered by `design-producing-tasks`).
- Inline / threaded comments on rendered docs (covered by archived `proj_18a2bb78815be670_3`).
- Cross-repo dispatch / multi-repo workspace orchestration (covered by `proj_18a2bbe20fc03718_8`).
- Multiple design docs per project.
- Watching / auto-updating pointers on rename or move.
- Multi-user permission / approval workflow on `SetProjectDesignDoc`.
- Reverse index ("which project owns this doc?") — naive scan suffices for v1.
- Non-markdown design artefacts (Figma, Notion, Google Docs). Forced markdown for v1; if the work environment really wants a Notion link, we relax the `.md` check and let the open affordance fall straight to web URL.
