# Boss: Optional Repo on Product + Per-Work-Item Repo Override

## Problem

Boss today treats every work item as belonging to *one* repo, derived from the product it sits under. `products.repo_remote_url` is nominally `Option<String>` in the schema (`tools/boss/engine/src/work.rs:1579`) but every code path treats it as effectively required: the reconciler at `work.rs:675` reads `product.repo_remote_url`, threads it into `reconcile_work_item_execution`, and if it's `None` for a new execution the function silently returns without creating a row (`work.rs:2926`). The cube dispatch path (`coordinator.rs:743`) then leases against that exact URL.

This is the right shape for the two products that exist today:

- **Boss** itself — a single mono repo at `git@github.com:spinyfin/mono.git`. Every Boss task / chore touches that repo.
- **Flunge** — likewise a single repo.

It is the wrong shape for the user's day job, where a single coherent "product" (the job, the platform, the team's surface area) spans dozens of git repos with no single integration repo. Forcing one Boss product per repo there would shred the kanban into many tiny columns that share no real organising principle — the user thinks in terms of *the work*, not *the repo it touches*. And it would produce a kanban full of products that all mean "work" without disambiguating what each one is *for*.

This doc proposes a small extension: make `repo_remote_url` truly optional on the product, and let each individual work item carry its own `repo_remote_url` override that wins when set. Resolution at dispatch time is a single fallback chain:

```
work_item.repo_remote_url ?? product.repo_remote_url
```

If both are `NULL` → the work item cannot dispatch; surface a clear error and ask the human to set one.

This preserves the existing Boss / Flunge behaviour byte-for-byte (the product carries the repo, work items inherit, the override column is `NULL` everywhere), and unlocks the multi-repo case (the product has no repo, every chore / task names its own). A mixed mode — a product with a default repo plus the occasional cross-repo chore that overrides it — also falls out for free.

## Goals

- Make `products.repo_remote_url` formally and functionally optional, with no dispatch attempted when it's `NULL` and no work item carries an override.
- Add `tasks.repo_remote_url` (the per-work-item override). `NULL` means "inherit from product."
- One resolution function (`resolve_repo_for_work_item`) used by every dispatch, listing, and UI surface — there is no place where the rule diverges.
- CLI verbs to set / unset the override on creation and after, plus a `--repo` filter on listings.
- macOS kanban surfaces the repo when it isn't fully determined by the parent product (i.e. when the override is set, or when the product has no default).
- Creation flow that "does the right thing" for the common case: explicit `--repo`, parsed from the prompt text, recent-context memory, product default, ask-once-or-fail.
- Dispatcher gracefully refuses unresolvable items with a clear, fixable error — no silent skip.
- Cube workspace pool config gets a clean "this repo isn't in any pool yet" bootstrap path.
- Backwards-compat is total: existing rows default to `NULL` override and keep dispatching against the product default. No CLI, RPC, or app caller in the Boss / Flunge case sees behavioural drift.

## Non-Goals

- **Multi-repo *within* a single work item.** A chore that touches two repos in one run is out for v1. The proposed model resolves to exactly one repo; a chore that legitimately spans two becomes two chores with a dependency edge between them (`work-dependencies.md`).
- **Cross-product references.** Work items in product A linking to / depending on work items in product B is a different concern; same parent project (`proj_18a2bbe20fc03718_8`) tracks it, but a separate design doc covers it once this lands. The dependency design's same-product constraint stays unchanged.
- **A separate `repos` table / repo registry.** We thought about modelling repos as first-class rows with their own ids, descriptions, short-names, and pool config. It's appealing but premature — the schema we already have (`products.repo_remote_url`, plus the new `tasks.repo_remote_url`) is two text columns and a fallback rule, and cube already owns the repo→pool mapping (`cube repo list`). Promoting to a table is a follow-up if the URL-as-key pattern hurts.
- **Auto-detection of "which repo does this prompt touch" via LLM call.** Q4's parser is regex / known-name matching only. Calling out to an LLM at chore-create time is overkill given the small number of repos any one product realistically references.
- **Renaming `repo_remote_url`.** The column name stays as-is on `products`; the new column on `tasks` matches the name exactly so the resolution code reads naturally.
- **Cross-repo workspace dispatch in the same lease.** Cube leases one workspace per repo per lease. A work item with an override against repo X gets a lease against repo X — full stop.

## Naming

- **Field name on `tasks`**: `repo_remote_url`, same as on `products`. `NULL` = inherit from product. We don't call it `repo_override` because the resolution rule is "the work-item value wins iff set" — it's not a true override of an underlying setting, it's the leaf in a two-step fallback. Using the same name on both rows lets the resolver read like prose: *"work_item.repo_remote_url or product.repo_remote_url."*
- **Resolution function**: `WorkDb::resolve_repo_for_work_item(work_item_id) -> Result<ResolvedRepo>` where `ResolvedRepo` is either `Some(url)` or a typed `RepoResolutionError::NoRepo`. The error is fatal at dispatch but recoverable in the UI ("click here to set one").
- **CLI flag**: `--repo <url>` on every create / update verb. `--repo ""` means "explicitly clear" (used on `boss product update` to remove the default, and on `boss chore update` / `boss task update` to revert to inheriting). The empty-string form is the same shape as `boss project update --pr-url ""` for clearing.
- **Filter flag**: `--repo <selector>` on every `list` verb. Selector matches against the *resolved* repo, not the raw column, so `--repo nimbus` finds work items that *effectively* run against `nimbus` regardless of whether the override is set explicitly or inherited.
- **Short name**: cube already uses a `<REPO>` argument (`cube workspace lease --task '...' <REPO>`) that takes either the canonical repo identifier (e.g. `mono`) or a full URL. Boss stores URLs; the CLI / UI derive the short name for display (basename of the URL, without `.git`). No registry; just a parse.

---

## Design Question 1 — Schema

### Options

- **(a) New nullable text column on each work-item table.** `tasks.repo_remote_url TEXT NULL`. Mirrors `products.repo_remote_url`.
- **(b) Separate `work_item_repos` join table** keyed by `(work_item_id, kind)`. Generalises to multiple repos per work item in a future v2.
- **(c) JSON column** carrying `{repo_remote_url, branch?, …}`. Forward-compat with adding branch / subpath qualifiers.
- **(d) Promote to a `repos` table** with stable ids; tasks and products carry a foreign key.

### Discussion

(b) is the right shape *if* and only if we expect multiple repos per work item in v2. The non-goals rule that out — and even when it changes, two chores linked by a dependency are a cleaner shape than one chore with two repos. (b) buys flexibility we explicitly don't want.

(c) wraps a small structured value behind JSON. Branch / subpath aren't load-bearing for dispatch (cube leases the repo's main branch checkout by convention, then the worker `jj git fetch` && `jj new <branch>` if it cares about a different branch — there's no per-work-item branch in v1). Storing JSON costs us indexability for `--repo` filters and gives us no real win.

(d) is the table-promotion path the *Non-Goals* section already declined. It's reversible: a future migration can carve a `repos` table out of the column if it pays for itself.

(a) is the natural extension of how `products` already shapes the repo. One column, nullable, defaults to `NULL`. Indexable for the listing filter. Trivial migration. Mirror name on both row kinds reads as a fallback chain.

### Recommendation

**Pick (a).**

```sql
ALTER TABLE tasks ADD COLUMN repo_remote_url TEXT;  -- NULL → inherit product.repo_remote_url
```

`tasks` carries both `kind = 'chore'` and `kind = 'project_task'` / `kind = 'design'` rows, so one column covers chores, project tasks, and design tasks uniformly.

```sql
CREATE INDEX IF NOT EXISTS tasks_repo_idx
    ON tasks(repo_remote_url, deleted_at)
    WHERE repo_remote_url IS NOT NULL;
```

Partial index because the override column is `NULL` for the overwhelming majority of rows in the Boss / Flunge cases, and SQLite's partial indexes are cheap when most rows are excluded. The `--repo` filter and "what work items live in repo X" introspection both hit this index.

The `products` table is unchanged: it already permits `NULL` (`work.rs:1579`). The only schema change is the additional column on `tasks`.

### Bumping the schema version

`metadata.schema_version` goes from `4` to `5`. The migration is purely additive (an `ALTER TABLE ADD COLUMN`), reversible by setting the new column to `NULL` everywhere and dropping it. Existing data is untouched: every chore / task / design row defaults to `repo_remote_url = NULL`, inheriting from product as before.

### Why not also add the column to `projects`?

Q2 covers this — short answer: project-level override is an attractive nuisance. Tasks under the project inherit either from the project (one extra layer) or directly from the product (the current shape, simpler). A two-layer fallback is harder to reason about than a one-layer one, and the v1 use case (a work product with no default repo and per-task overrides) doesn't need it. If a project legitimately spans repos, that's *one project per actual cluster of work*, and the work items carry the repo, not the project.

---

## Design Question 2 — Projects

### The question

Does `projects.repo_remote_url` also exist? If so, do tasks under that project resolve `task → project → product` (three-layer fallback) or stay `task → product` (two-layer, project ignored)?

### Options

- **(α) No project-level override.** Projects ignore repo entirely; tasks inherit straight from product.
- **(β) Project-level override, two-layer.** A task with no `repo_remote_url` falls back to `project.repo_remote_url`, then `product.repo_remote_url`.
- **(γ) Project-level override that *replaces* the product default for that project's tasks.** Different semantics: setting `project.repo_remote_url = foo` *forces* every child task to use `foo` unless the task explicitly carries its own override.

### Discussion

(γ) is a footgun: it sneaks a "lock all tasks to this repo" semantic into a field that looks identical to (β). Reject.

(β) generalises the model symmetrically — every row kind that contains work has an optional repo, and resolution walks up the tree. The cost is one more layer in the resolver and one more place to look when debugging "why is this task dispatching to the wrong repo." That cost is real: future-Brian (or a worker session reading the rules) has to reason about three potential sources for the same value.

(α) keeps the resolver flat. A project is a *grouping of design + tasks*, not a repo declaration. The motivating use case ("at work, the product has no repo and each chore picks its own") doesn't involve projects — chores live directly under the product. The case where it *would* matter — a project whose tasks all touch one repo that's distinct from the product default — is handled cleanly by either (1) setting each task's `repo_remote_url`, which is a few keystrokes per task and a one-shot batch via `boss task create-many`, or (2) by making that "project" a separate Boss product with its own repo, which is its actual semantic shape.

I considered the third reading: a project that *describes design and implementation that crosses repos*. The user has at least one of these (the multi-repo project itself!). But the tasks within such a project each touch a specific repo, not "the project's repo" — so per-task overrides win there too.

### Recommendation

**Pick (α). No project-level override in v1.**

Tasks under a project follow the same resolver as chores: `task.repo_remote_url ?? product.repo_remote_url`. The project row is *transparent* in the resolution chain.

This keeps the resolver one-line and the mental model crisp: *"the only places a repo can be set are on the product (default) and on the work item (override)."*

If a real case for project-level override appears later, this is purely additive — one ALTER TABLE, one extra fallback step in `resolve_repo_for_work_item`, no caller has to change.

### What about design tasks?

`kind = 'design'` rows are tasks; they sit in the same `tasks` table as project tasks and chores. They get the column for free under recommendation (a). For the at-work multi-repo product, that's important: a design task may produce a doc *outside* any of the work product's chore repos (e.g. in a wiki repo). That works today using `design-producing-tasks.md`'s `products.docs_location` field for the doc location, and orthogonally now the design task can carry its own override for the repo it dispatches into. The two fields are independent: `tasks.repo_remote_url` is "where do I work," `products.docs_location` / `tasks.metadata.design.doc_branch` is "where do I write the doc." They can name the same repo or different repos; the resolver doesn't conflate them.

---

## Design Question 3 — Repo Identity

### The question

What goes into the column? A remote URL (`git@github.com:spinyfin/mono.git`), a short name (`mono`), or both?

### Options

- **(I) Full remote URL.** Unambiguous; verbose; one canonical form to canonicalise.
- **(II) Short name.** Compact; readable; requires a registry mapping `mono → git@github.com:spinyfin/mono.git`.
- **(III) Tagged union.** Store one of the two, parse on read.

### Discussion

Cube's CLI takes either: `cube workspace lease --task '...' <REPO>` accepts the short name (the column it uses as the stable repo identifier) and `cube repo ensure --origin <URL>` accepts the URL. Internally cube maps URL ↔ short-name via its `cube_repos` table.

(II) is friendlier in the UI ("`nimbus` chip on a card" beats "`git@github.com:myorg/nimbus.git` chip") but requires Boss to either *be* the registry or *trust* cube's registry as a source of truth. Both are wrong: the registry that cube already maintains is for the workspace pool, not for "which repo URL is short-named what." If we make Boss its own registry we duplicate state; if we delegate to cube the column is meaningless without a live cube DB to resolve against. Either failure mode is worse than just storing the URL.

(III) papers over the choice. The column's parser sometimes does and sometimes doesn't have a URL; downstream code can't tell which without a `match`. That bleeds across every consumer.

(I) is unambiguous. The trade-off — verbose values in DB rows — is paid once at write time and at debug time; UI surfaces derive a short name on read with a tiny parser (the GitHub URL form is `git@host:owner/repo.git` or `https://host/owner/repo.git`; the basename minus `.git` is the short name).

### Recommendation

**Pick (I).** Store the canonical remote URL.

```rust
fn short_name_for(url: &str) -> &str {
    // Strip protocol + host, take the path basename minus `.git`.
    // git@github.com:foo/bar.git → bar
    // https://github.com/foo/bar.git → bar
    // ssh://git@host/foo/bar     → bar
    let s = url.rsplit('/').next().unwrap_or(url);
    s.rsplit(':').next().unwrap_or(s).trim_end_matches(".git")
}
```

Canonicalisation happens once at write time (`SetRepoForWorkItem` and `CreateProduct` / `UpdateProduct` both go through the same canonicaliser; see Q8). The CLI accepts any of the common URL forms (`https://github.com/foo/bar`, `https://github.com/foo/bar.git`, `git@github.com:foo/bar.git`) and writes one canonical shape. The same canonicaliser already lives in `engine/src/work.rs` for `products.repo_remote_url`; we lift it into a shared helper and call it from both surfaces.

### What about the `--repo <selector>` filter?

The filter accepts *either* a short name (matched against `short_name_for(url)` on every row's resolved repo) or a full URL (matched against the canonicalised form). This is the only place where short-name → URL "resolution" happens, and it's a `WHERE LOWER(short_name_for(...)) = LOWER(?)` not a global registry. A short-name collision across two different URLs (`foo/nimbus` vs `bar/nimbus`) becomes a single match for the filter, which is the right UX — the user typed `nimbus`, they get all `nimbus`.

If a collision becomes painful in real life, the filter accepts the disambiguating full URL and the user moves on. Boss does not need to police it.

---

## Design Question 4 — Creation-time Repo Resolution

### The cases

When the user runs `boss chore create --product work` (no `--repo`), we have three signals to consult, in order:

1. **The prompt text.** *"In the nimbus repo, please fix the deploy script"* — the user said the repo out loud.
2. **Recent-context memory.** The last repo this product's work items used. Cached cheaply.
3. **Product default.** `product.repo_remote_url`, if set.
4. **Ask once interactively, or fail in `--no-input`.**

If after these signals the column is still ambiguous, the create verb refuses with a clear message; the user re-runs with `--repo`.

### Parser scope

The prompt-text parser is the only piece that needs design care; (2) and (3) are SQL lookups.

The parser is a small, regex-driven matcher against the product's known repo set:

```rust
/// All distinct repo URLs that appear on any work item under
/// `product_id`, plus `product.repo_remote_url`. Drives the
/// prompt-text inference at chore / task create time.
fn known_repos_for_product(conn: &Connection, product_id: &str) -> Result<Vec<String>>;
```

For each known URL, derive `short_name_for(url)` and a few alternates (full URL, the path component `owner/repo`, the short name minus dashes). Then case-insensitive substring-search the prompt for any of those.

Two-step match:

- **Exact-token match wins.** `"in the nimbus repo"` matches the short name `nimbus`.
- **`in <name>`, `<name> repo`, `<name>/...` patterns** as a fallback. Same regex shape as the `git@host:<owner>/<repo>` extractor.

If multiple known names match, pick the *first one mentioned in the prompt* (left-most position wins). If two short names collide on the same position (rare, but `nimbus-frontend` and `nimbus`), pick the longer match.

If zero match → fall through to step (2).

The parser is regex / substring matching only. **No LLM call.** A future enhancement could ask the worker model when it routes the chore, but the latency / cost / opacity is wrong for chore-create.

### Recent-context memory

```sql
SELECT repo_remote_url FROM tasks
WHERE product_id = ?
  AND repo_remote_url IS NOT NULL
  AND deleted_at IS NULL
ORDER BY updated_at DESC
LIMIT 1;
```

Single row, partial index helps. "Last repo the human used" is a strong default; the user is usually in a working session pinned to one repo for a stretch.

If even this returns nothing (the product has never had a per-work-item override and no chore explicitly carries one), step (3) applies.

### Step (3): product default

If `product.repo_remote_url` is set, use it. This *is* the Boss / Flunge case — there is no inference because the answer is unambiguous.

### Step (4): ask or fail

If steps 1–3 all whiffed, the CLI behaviour is:

- **Interactive mode (`isatty` and no `--no-input`).** Prompt: *"No repo could be resolved. Known repos for `work`: nimbus, ledger, console. Pick one or enter a full URL:"*. The list shows the same `known_repos_for_product` set, plus an "Enter URL…" option. The chosen value is written to the new chore, *not* to the product default — the product genuinely has no default in this scenario.
- **`--no-input` mode (the engine / scripted callers).** Error: *"could not resolve repo for new chore under product `work` (product has no default; prompt mentions no known repo; no prior work-item repo cached). Re-run with `--repo <url>` or set a product default with `boss product update work --repo <url>`."* Exit non-zero. The engine sees this as an `invalid_request`.

### How the known-repo set is populated

It's not a manually-maintained registry. The set is the *empirical* distinct-URL set across all of a product's tasks plus the product default — which means the first time a user creates a chore against a new repo, the parser misses it (because nothing else under the product references that repo yet); the user supplies `--repo` once; subsequent chores can be auto-inferred from the prompt because the URL is now in the empirical set.

This bootstraps cleanly from zero. The drawback: a brand-new product with no chores and a wiki-style prompt-text inference *cannot* work — the parser has nothing to match against. That's fine because step (4) catches it (ask the human once).

### Recommendation

**Resolver order: explicit `--repo` → prompt parser → recent-context → product default → ask-or-fail.**

Implemented as a single helper:

```rust
fn resolve_repo_at_create_time(
    conn: &Connection,
    product_id: &str,
    explicit_flag: Option<&str>,
    prompt_text: &str,
    interactive: bool,
) -> Result<Option<String>>;
```

`None` on `interactive = false` triggers an error in the caller; `None` on `interactive = true` only happens after the user explicitly skips the prompt (Ctrl-C).

---

## Design Question 5 — Worker Dispatch

### The change

Cube workspace lease (`cube workspace lease --task '...' <REPO>`) takes one repo argument. Today the dispatcher passes the execution row's `repo_remote_url` (`coordinator.rs:743`):

```rust
self.cube_client.ensure_repo(&execution.repo_remote_url).await
```

`work_executions.repo_remote_url` is already populated *per execution row* by `reconcile_work_item_execution` (`work.rs:2935`), which gets it as a `repo_remote_url: Option<&str>` argument from the per-product reconciler (`work.rs:675`). Three changes needed:

1. **Resolution at reconcile time.** `reconcile_work_item_execution` should look at the *work item's* `repo_remote_url` first, then the product's. New helper `resolve_repo_for_work_item(conn, work_item_id) -> Option<String>`; the existing per-product reconciler stops threading `product.repo_remote_url` as the only signal and instead per-row resolves.
2. **No-repo path.** Today `reconcile_work_item_execution` returns `Ok(())` silently when `repo_remote_url` is `None` (`work.rs:2926`). Replace the silent skip with a `WorkAttentionItem` recording *"work item X has no repo resolution; set one with `boss <kind> update --repo <url>` or set a product default."* The item is sticky until the user fixes the row. Existing dispatch flow is unchanged for any row whose repo *does* resolve.
3. **Pool config.** Cube's pool configuration is per-repo. The engine's `ensure_repo(&url)` call (`coordinator.rs:745`) already handles this — cube auto-materialises a pool from the origin URL if none exists. So the dispatch story for a brand-new repo (no pool yet) is: the engine asks cube to ensure it, cube creates the pool, the lease proceeds. That's the existing flow; no engine change here.

### The "where does the pool come from" question

The work item description asks: *"Pool config currently assumes one product → one repo pool. Spec how a repo-less product's work items find the right pool."*

This is moot under the actual cube model. Pools are keyed by *repo*, not by *product*. `cube repo ensure --origin <URL>` materialises a pool for a URL; `cube workspace lease --task '...' <REPO>` leases against the pool. Boss has never enforced "one product → one pool" except by virtue of every product carrying one URL. With per-work-item URLs, the same cube machinery applies: the dispatcher resolves URL → pool via `cube repo ensure` (which is idempotent and cheap) every time it dispatches, regardless of which row the URL came from.

The only operational concern is *workspace count*. Each repo's pool has a workspace count; for a multi-repo product, the engine may want to keep more workspaces alive across repos than for a single-repo product. That's a cube configuration concern, not a Boss one — set the per-repo `workspace_count` in cube to taste. We document it in the bootstrap note (Q6) but don't model it.

### Resolver and the explicit `RequestExecution` path

`bossctl work start <id>` (the explicit path) currently hits `request_execution_in_tx_with_live_check` (`work.rs:2953`), which calls `insert_execution` to create the row. That helper too needs the resolution — we route it through the same `resolve_repo_for_work_item` so a `boss work start` against an unresolvable row refuses with the same error that the reconciler would have surfaced.

### Recommendation

```rust
fn resolve_repo_for_work_item(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    // task or chore: check tasks.repo_remote_url, fall back to product.
    // No project layer (Q2).
    if let Some(row) = query_task_row(conn, work_item_id)? {
        if let Some(url) = row.repo_remote_url.as_deref().filter(|s| !s.is_empty()) {
            return Ok(Some(url.to_owned()));
        }
        let product = query_product(conn, &row.product_id)?
            .with_context(|| format!("orphan task {work_item_id}: product missing"))?;
        return Ok(product.repo_remote_url);
    }
    // project: tasks under the project inherit from product (Q2), but the
    // project row itself doesn't dispatch — only its tasks do. So this
    // branch only fires for design tasks, which are tasks above.
    Ok(None)
}
```

Failure shape on unresolvable dispatch:

```rust
match resolve_repo_for_work_item(&conn, work_item_id)? {
    Some(url) => proceed_with(url),
    None => {
        // Sticky attention item; no execution row.
        record_attention_item(
            conn,
            work_item_id,
            "repo_unresolved",
            "Set a repo with `boss <kind> update --repo <url>` or a product default.",
        )?;
        return Ok(());
    }
}
```

The attention item flows through the existing `WorkAttentionItem` surface (kanban "Attention" lane + `boss attention list`).

---

## Design Question 6 — Workspace Bootstrap for Cold Repos

### The question

When the user has never run a worker against a particular repo (e.g. the work product's `nimbus` repo), does dispatch Just Work, or does it need an admin step?

### Current cube behaviour

`cube workspace lease` already auto-creates a workspace if the pool is exhausted (`v2-design-risks.md:546`). `cube repo ensure --origin <URL>` materialises a pool if one doesn't exist. So in principle the chain is:

```
dispatch
  → cube repo ensure <url>     (idempotent; creates pool if missing)
  → cube workspace lease       (auto-creates a workspace if pool empty)
  → workspace ready
```

The gap is in cube's `repo ensure` — it accepts `--origin` plus a stable identifier (the `<REPO>` argument to e.g. `cube repo add`). When the engine calls `ensure_repo(&url)` (`coordinator.rs:745`), cube extracts a default short identifier and provisions a pool with default `workspace_root` / `workspace_prefix` derived from the URL. That works today for the existing single-pool case. For a brand-new repo with no `cube repo add` ever run, the auto-provision uses defaults that may not match the user's filesystem conventions.

### Options

- **(A) Trust cube's auto-provision for cold repos.** No Boss change. First worker dispatch against a new repo triggers cube's `repo ensure`, which creates a pool with defaults. The user can `cube repo add <short> --origin <url> --workspace-root <dir> --workspace-prefix <prefix>` later to retroactively configure if defaults are wrong.
- **(B) Require a `cube repo add` for every new repo before Boss can dispatch against it.** Bossctl checks `cube repo list` on first dispatch; if missing, refuses with *"run `cube repo add <short> --origin <url> --workspace-root <dir> --workspace-prefix <prefix>` first."*
- **(C) Surface a "cold repo" attention item but allow dispatch.** First dispatch proceeds with cube defaults; an attention item flags that the workspace_root might be unexpected, and the user can opt into a custom configuration.

### Discussion

(A) is the path of least friction. The downside is that the user may end up with workspaces in a default location that doesn't match their conventions (e.g. `~/Documents/dev/workspaces/` for the rest of their tooling, vs cube's default of `~/.cube/workspaces`). For a single-user setup this is annoying but recoverable: `cube repo add` later overrides the pool config and the next workspace gets created in the right place; the original mis-located workspace can be `cube workspace remove`d.

(B) is the most explicit but breaks the smooth UX of "ask the AI to fix something in a new repo, it just works." For the work case where there might be many repos to register, this is meaningful friction.

(C) attempts the best of both. It costs an attention-item surface but lets the user keep moving.

### Recommendation

**Pick (C).** Add a "cold repo" attention item the first time a previously-unseen repo URL appears in dispatch, with the body:

```
First dispatch against `nimbus` (git@github.com:myorg/nimbus.git).
Cube auto-provisioned a pool at <default_workspace_root>.
To customize, run:
    cube repo add nimbus --origin git@github.com:myorg/nimbus.git \
                       --workspace-root ~/Documents/dev/workspaces \
                       --workspace-prefix nimbus-agent
```

The detection is: on every successful `ensure_repo` call, the engine asks cube `repo list --json` (one round-trip, ~1ms) and checks whether the returned row's `workspace_root` was the cube default or human-configured. If default, raise the attention item once per repo (deduped by repo URL).

The attention item is *advisory*, not blocking. The user can ignore it and dispatch keeps working with cube defaults.

If the user resolves the attention item (`boss attention resolve <id>`) it doesn't reappear. If they re-add the repo with custom config, the resolver no longer sees the "default config" state and never raises it again.

### What about workspace count?

Each pool has a configurable workspace count (concurrent workers per repo). For multi-repo products this matters: dispatching against five repos simultaneously needs five times the concurrency cap. The engine doesn't enforce or configure this; documented in the cold-repo attention item as a follow-up the user can tune in cube.

---

## Design Question 7 — UI Surfacing

### Kanban card

Today's card layout (`WorkBoardCardView`, `app-macos/Sources/ContentView.swift`) shows a project tag, the title, the status pill, and a PR link in the footer. For multi-repo products, the *repo* becomes a load-bearing field that the user needs to scan at a glance.

Three options:

- **(P) Repo chip in the card header.** Right of the project tag, before the title. Mirrors the project chip's shape.
- **(Q) Repo chip in the footer.** Smaller, alongside the PR link.
- **(R) No chip; rely on the title to say it.** The cheapest option but defeats the point.

### Recommendation

**Hybrid:**

- **When the product has a single resolved repo** (i.e. `product.repo_remote_url` is set AND no card's `task.repo_remote_url` deviates) — *no chip*. The whole column shares the repo; printing it on every card is noise. The product header lane (top of kanban) shows the repo once.
- **When the product has no default OR any card overrides** — *show the chip on every card* (option P, in the header). The repo chip carries the short name (`short_name_for(url)`). Two chips with different repos in the same column scan instantly; one of them being different from the rest is a visible signal.

Implementation lives in `WorkBoardView`'s render loop: the column computes `effective_repo_for_product = if all cards resolve to the same URL { Some(url) } else { None }`. If `Some`, the column header carries the chip and per-card chips are suppressed; if `None`, per-card chips appear.

Hover on the chip shows the full URL.

### `boss <kind> list` text output

Add a `Repo` column (after `Status`, before the dependency-related columns) showing the short name. Sortable, filterable via `--repo`.

For a single-repo product (every row resolves to the same URL), the column collapses to a one-line legend at the top of the table — *"All rows under `boss` resolve to `mono`."* This is consistent with how the kanban handles it.

### `boss <kind> show` output

Add a *Repo* line near the top, after the *Status* line:

```text
Repo:    git@github.com:myorg/nimbus.git  (override on this work item)
         git@github.com:spinyfin/mono.git (inherited from product `boss`)
```

The parenthetical explains *which row supplied the value* — a small UX win for debugging "why is this dispatching against the wrong repo." If neither row supplies one, the line reads *"(none — work item cannot dispatch)"* and the show output gains a hint about how to set one.

### Recommendation

**Repo chip on the card header *only* when the product is multi-repo. Repo column on list verbs, with single-repo collapsing. Repo line on show output, always present, indicating the source row.**

---

## Design Question 8 — Backwards Compatibility

### Migration semantics

- Existing `tasks` rows: `repo_remote_url` defaults to `NULL` on the new column.
- Existing `products` rows: unchanged (the column was already `NULL`-permissive).
- Existing CLI invocations: no flag is required; `--repo` is optional.
- Existing API consumers: the new field appears in `Task` and `CreateChoreInput` / `CreateTaskInput`; older clients sending payloads without it are accepted (the field is `Option<String>` with `#[serde(default)]`).
- Existing app builds: a build that doesn't yet decode the new field gets `Option<String>` defaulted to `None` on the wire and renders the same as today.
- Dispatch behaviour for the Boss / Flunge case: byte-for-byte unchanged. Every row has `tasks.repo_remote_url = NULL`, the resolver falls through to `product.repo_remote_url` (which is set), the URL handed to cube is identical to today.

### Wire-shape guarantees

`Task` payloads from the engine gain a new `repo_remote_url: Option<String>` field with `#[serde(default)]`. Old clients that decode `Task` and don't know the field still parse the payload (serde ignores unknown fields by default unless `#[serde(deny_unknown_fields)]` is set, which Boss does not set on `Task`).

Old clients sending `CreateChoreInput` / `CreateTaskInput` without the field get a chore / task with `repo_remote_url = NULL`. The resolver inherits from the product as before. No behavioural difference.

### Recommendation

Pure additive migration; no behavioural change for existing rows. Document in `tools/boss/CHANGELOG.md` (or the equivalent release notes) that v2 of the work-item shape includes a per-row repo override.

---

## Design Question 9 — CLI Surface

### Verbs

```
boss chore   create  --product <p> [--repo <url>] --name "..." --description "..."
boss task    create  --product <p> --project <pj> [--repo <url>] --name "..." --description "..."

boss chore   update  <selector> [--repo <url> | --repo ""]
boss task    update  <selector> [--repo <url> | --repo ""]

boss product update  <selector> [--repo <url> | --repo ""]
boss product create  --name "..." [--repo <url>]      # already exists; --repo already supported

boss chore   list    [--repo <selector>]
boss task    list    [--repo <selector>]
boss project list    [--repo <selector>]
```

`--repo <url>`: set / override the repo for this row.
`--repo ""`: explicitly clear the override (revert to inheriting). Same shape as `--pr-url ""` already supported on `boss <kind> update`.
`--repo <selector>` on list verbs: filter by *resolved* repo (matches short name or full URL, per Q3).

### Showing the resolved repo

`boss <kind> show <selector>` gains the "Repo:" line (Q7).

### Reference / help text

`boss reference` gains:

> A work item's resolved repo follows `task.repo_remote_url ?? product.repo_remote_url`. Set the override with `--repo <url>` on create or update; clear it with `--repo ""`. A work item with no resolution refuses to dispatch and surfaces an attention item.

### What about `boss product create --repo ""`?

The task-spec asks for this. `boss product create` already accepts `--repo <url>` as optional; `--repo ""` is the explicit "create the product with no default repo" form. Currently `CreateProductInput.repo_remote_url` is `Option<String>` so omitting `--repo` is identical to passing `--repo ""`. We keep both forms accepted to give the user a way to make the intent explicit when scripting.

### Recommendation

Verb signatures as above. The empty-string form (`--repo ""`) consistently means "clear / no override / no default." Implementation routes both forms through the same `normalize_optional_text` helper that already handles the analogous case for product description fields.

---

## Design Question 10 — macOS App UI

### Product create form

The current form has a *Repo* field that's required. Change to optional, with helper text *"Leave empty if work items will specify their own repo."*

### Work-item create form (chore + task)

Today there's no repo field on the chore/task create form because the product carries it implicitly. With the override column added:

- **When the parent product has a default repo:** the form hides the repo field by default, with a *"Override repo…"* disclosure that expands a repo picker. The picker is pre-populated with the known-repo set from `known_repos_for_product` (Q4), with a *"Custom URL…"* option at the bottom.
- **When the parent product has no default:** the repo field is shown by default, required, with the same picker as above plus a *"Set as product default"* checkbox that, if checked, sets `product.repo_remote_url` in the same submit.

### Recent-repos picker

Populated from the per-product distinct-URL query. Cached in the app's view model; refreshed on every `WorkTree` update (the snapshot already includes every task's `repo_remote_url`, so the recent set is derived client-side without a new RPC).

### Editing an existing work item

The chore / task detail surface (when it exists; today it's a popover) gains a *Repo* row that shows the resolved repo with a parenthetical source (Q7). Clicking the row opens the same picker.

### macOS kanban repo chip

Implementation outlined in Q7. The chip renders as an `SF Symbol` (a folder or branch icon) plus the short name in monospace; hover tooltip shows the full URL.

### Recommendation

Product form: repo field optional with helper text. Work-item form: repo field shown when product lacks a default, hidden behind a disclosure when product has one. Picker reuses the per-product known-repo set. No new RPC needed; everything's derivable from `WorkTree`.

---

## Design Question 11 — Edge Cases

### Renamed repo (URL changes upstream)

A repo gets renamed on the host (`myorg/nimbus` → `myorg/nimbus-platform`). The stored URL points at the old name. GitHub redirects on HTTPS pulls / pushes; SSH redirects exist but are less reliable. Cube's local checkout doesn't auto-rename. The user must run `boss chore update <selector> --repo <new_url>` (and the corresponding `cube repo add` to update the pool's origin).

**Recommendation:** no auto-detection; the rename is a manual operation. Document in the CLI help.

### Repo moved between hosts

Same answer as rename. Manual `boss <kind> update --repo`.

### Repo archived (host-side)

Doesn't affect the stored URL. Cube workspace operations against an archived repo may fail (push-side specifically). The user updates the override if they need to retarget; otherwise the dispatch error surfaces in the cube lease step and lands in the existing run-failure surface.

### Repo not yet cloned into any cube pool

Covered by Q6 — cube auto-provisions on first `ensure_repo`. Attention item flags the default config.

### Two repos with the same short name in different orgs (`foo/nimbus` vs `bar/nimbus`)

URL is the canonical key; the column always stores URL. Filter `--repo nimbus` matches both — the user sees both in the result and disambiguates with `--repo git@github.com:foo/nimbus.git`. UI chip shows the short name for both; users with collisions can hover for the full URL.

### Work item created with `--repo <url>` whose URL is malformed

Reject at create time. Same validator as `boss product create --repo <url>` today, lifted into a shared helper. Error message includes the expected forms.

### Existing chore migrated from the Boss / Flunge case to set an explicit override

The user runs `boss chore update <selector> --repo <new_url>`. The new URL canonicalises, the column updates, the next reconcile dispatches against the new URL. The previous in-flight execution (if any) keeps its existing `work_executions.repo_remote_url` value — execution rows are snapshots, not pointers; the next dispatch creates a fresh execution row carrying the new URL.

### Setting an override to the same URL as the product default

Allowed. Stored as the literal URL. Functionally equivalent to no override. The UI doesn't show a chip because the resolver result equals the product default, satisfying the "show chip iff card differs" rule from Q7.

The CLI could warn — *"override matches product default; you could clear with `--repo \"\"`"* — but doesn't, because the user may be intentionally pinning to a URL they expect to differ from the product default someday.

### Resolving repo for a work item whose product was deleted

`product` should always exist for an existing work item (foreign key). If somehow it doesn't, the resolver returns an error (`orphan task: product missing`), which lands as an attention item the same way an unresolvable dispatch does.

### `boss task list --repo <selector>` when the selector matches zero repos

Returns an empty list. Same shape as any other filter that matches nothing.

### Resolver and the per-execution repo snapshot

`work_executions.repo_remote_url` is `NOT NULL` (`work.rs:1632`). The reconciler writes the *resolved* URL at execution-creation time. Once a row is in `running` / `completed` / `failed`, the URL is frozen — even if the user changes the override or the product default afterwards, the execution row still shows the URL it ran against. This is correct: executions are history.

### Work item with override pointing at a repo cube can't reach

Cube returns an error from `ensure_repo` or `lease_workspace`. The engine's existing error path (`coordinator.rs:783` — *"cube workspace lease failed; marking execution start as failed"*) catches it. The work item moves to a failure state with the cube error verbatim. The user diagnoses and updates the override.

### Multi-repo product where one repo's pool exhausts

Cube's per-repo workspace count caps per repo, not per product. A multi-repo product whose one repo has a depleted pool sees that repo's work items queued at `ready` while other repos' work items dispatch normally. This is correct — the cap is a per-repo resource concern.

---

## Schema and Wire Summary

### Column add

```sql
ALTER TABLE tasks ADD COLUMN repo_remote_url TEXT;  -- NULL → inherit product.repo_remote_url

CREATE INDEX IF NOT EXISTS tasks_repo_idx
    ON tasks(repo_remote_url, deleted_at)
    WHERE repo_remote_url IS NOT NULL;
```

`metadata.schema_version` bumps from `4` to `5`.

### Protocol additions (`tools/boss/protocol/src/types.rs`)

```rust
pub struct Task {
    /* … existing fields … */
    /// Per-work-item override. `None` → inherit from the parent
    /// `Product.repo_remote_url`. Stored as a canonical remote URL
    /// (e.g. `git@github.com:myorg/repo.git` or
    /// `https://github.com/myorg/repo.git`); short-name display is
    /// derived on the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

pub struct CreateChoreInput {
    /* … existing fields … */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

pub struct CreateTaskInput {
    /* … existing fields … */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

pub struct WorkItemPatch {
    /* repo_remote_url is already present (line 332). Behaviour
       extends to tasks: `Some(s)` sets the override, `Some("")`
       clears, `None` leaves unchanged. */
}
```

Resolution helper (engine-only, not wire):

```rust
// engine/src/work.rs
pub fn resolve_repo_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>>;
```

### CLI verbs

```
boss chore   create  --product <p> [--repo <url>] --name "..."
boss task    create  --product <p> --project <pj> [--repo <url>] --name "..."
boss chore   update  <selector> [--repo <url> | --repo ""]
boss task    update  <selector> [--repo <url> | --repo ""]
boss product create  --name "..." [--repo <url>]              # already exists
boss product update  <selector> [--repo <url> | --repo ""]    # already exists
boss chore   list    [--repo <selector>]                       # new filter
boss task    list    [--repo <selector>]                       # new filter
boss project list    [--repo <selector>]                       # new filter
boss <kind>  show    <selector>                                # gains Repo line
```

### Engine module split

- `engine/src/work.rs` — add `resolve_repo_for_work_item`, update `reconcile_work_item_execution` and `reconcile_product_executions` to use it, update `request_execution_in_tx_with_live_check` to use it. Update `update_task` / `update_chore` to handle the `repo_remote_url` patch (the column write is one line; behaviour for empty-string → NULL matches existing `apply_optional_patch`).
- `engine/src/coordinator.rs` — the cold-repo attention-item probe in `schedule_execution` (Q6). Reads `cube repo list --json` once per repo URL, dedups by URL.
- `cli/src/main.rs` — `--repo` flag on the create / update / list verbs; `boss <kind> show` Repo line rendering; the prompt-text parser helper (Q4) lives in a sibling module (`cli/src/repo_resolution.rs`) and is invoked from `ChoreCommand::Create` and `TaskCommand::Create`.
- `app-macos/Sources/Models.swift` — mirror `Task.repo_remote_url`.
- `app-macos/Sources/ContentView.swift` — `WorkBoardCardView` repo chip (Q7); product / chore create forms (Q10); recent-repos picker.

### Topic / event

`work.tree` (the existing `WorkTreeUpdated` topic) covers it — `Task` payloads gain the new field, and any change to `repo_remote_url` already broadcasts via the same write path as `description` and `priority`. No new topic.

---

## Resolution Pipeline (combined diagram)

```
                     boss chore create
                          │
                          ▼
            ┌─────────────────────────────┐
            │ resolve_repo_at_create_time │
            │   1. --repo <url>           │   ← explicit
            │   2. prompt text parser     │   ← known-repos for product
            │   3. recent-context query   │   ← last task's repo
            │   4. product default        │   ← products.repo_remote_url
            │   5. ask-or-fail            │   ← interactive only
            └──────┬──────────────────────┘
                   │ chosen URL or NULL
                   ▼
            ┌──────────────────────────┐
            │ INSERT INTO tasks (...)  │
            │ repo_remote_url = ?      │
            └──────┬───────────────────┘
                   │
                   ▼
            (reconcile fires)
                   │
                   ▼
            ┌──────────────────────────────────┐
            │ resolve_repo_for_work_item(id)   │
            │   tasks.repo_remote_url          │
            │     ?? products.repo_remote_url  │
            └──────┬───────────────────────────┘
                   │
       ┌───────────┴────────────┐
       │ Some(url)              │ None
       ▼                        ▼
INSERT work_executions    record attention item
  repo_remote_url = url    'repo_unresolved' (sticky)
       │                        │
       ▼                        ▼
   dispatcher picks up      no dispatch; user fixes
   → cube ensure_repo url
   → cube lease_workspace
   → run
```

---

## Risks

**R1 — Resolution drift.** The resolver is called from at least three places (reconciler, explicit-request, attention probe). If they diverge, "why is this row dispatching against the wrong repo" becomes hard to debug. Mitigation: single helper `resolve_repo_for_work_item`, every caller routes through it; unit tests cover the three callers explicitly.

**R2 — Prompt-text parser false positives.** *"Don't touch the nimbus repo, leave it alone"* matches `nimbus`. The user expected step (3) (recent context) or step (4) (ask). Mitigation: the parser matches short-name *tokens* in prompt text; negation phrases are out of scope. If a user repeatedly hits this, they pass `--repo` explicitly or accept the friction. A future enhancement could bring in a small grammar; v1 doesn't.

**R3 — Empirical known-repo set bootstraps slowly.** A brand-new product has zero known repos and the prompt parser whiffs every time until the user manually `--repo`'s one chore. Acceptable; document in the help text. The "recent context" query covers it after one chore exists.

**R4 — Cold-repo attention spam.** A power user adds many new repos in a session; every one raises an attention item. Mitigation: dedup by URL (already in Q6); user can `boss attention resolve --all-of-kind repo_cold_pool` to clear in bulk.

**R5 — Cube default workspace_root mismatch.** The cube auto-provision uses defaults that don't match the user's filesystem conventions. The first workspace is in the wrong place. Mitigation: cold-repo attention item names the exact `cube repo add` command to override; once configured, subsequent workspaces are created correctly.

**R6 — Per-execution URL freeze.** An execution kicked off against the old URL keeps dispatching against the old URL even after the user updates the override. Mitigation: this is the correct behaviour (executions are history), but document on `boss <kind> show` that the work item's *current* repo and an in-flight execution's repo may differ — and that the next execution will use the new value.

**R7 — Empty-string vs NULL ambiguity.** SQLite distinguishes `''` from `NULL`; the resolver checks both (`s.filter(|s| !s.is_empty())`). If a caller mis-writes `''` we still inherit. Mitigation: `apply_optional_patch` normalises empty strings to `NULL` on write; this is already the pattern for the existing optional columns.

**R8 — A multi-repo product's tasks accidentally inherit `NULL` when the user expects inheritance from another work item.** The resolver doesn't walk between work items. Mitigation: documented in Q2 — the only fallback is `product`. Users who want sibling-inheritance create separate products. If a real pattern emerges (one design task supplies its repo to its sibling implementation tasks), it's a follow-up; not v1.

**R9 — App rendering mid-migration.** An app build that decodes `Task` and crashes on unknown field doesn't exist (serde / Codable default to ignoring unknown fields), but a build that *expects* the field and crashes on missing field could exist if we mark it required. Mitigation: `#[serde(default)]` and `Codable` with `decodeIfPresent`; existing wire-shape tests catch regressions.

**R10 — Filter behaviour on `--repo` with substring match.** Q3's filter says short-name match is case-insensitive substring. `--repo m` matches `mono`, `metrics`, and `samonette`. Mitigation: substring is *prefix*, not free substring; reject filters shorter than 2 chars to keep false-positive density low.

---

## Follow-up Implementation Chores (to enqueue once approved)

These are bite-sized so each fits in a single worker session.

1. **Schema + migration**: add `tasks.repo_remote_url`, partial index, bump `schema_version` to 5. Acceptance: fresh init + migration from v4 both yield the v5 schema; existing rows default to `NULL`.

2. **Protocol types**: `Task.repo_remote_url`, `CreateChoreInput.repo_remote_url`, `CreateTaskInput.repo_remote_url`. Mirror in `app-macos/Sources/Models.swift`. Acceptance: serde / Codable round-trips green; existing wire-shape tests still pass.

3. **Engine: `resolve_repo_for_work_item`** — single helper. Unit tests cover override-set, override-empty, override-NULL-product-set, both-NULL, orphan-product error. Acceptance: tests green; no engine route wired up yet.

4. **Engine: dispatch resolution** — `reconcile_work_item_execution` and `request_execution_in_tx_with_live_check` route through the resolver. When `None`, raise the `repo_unresolved` attention item instead of silently skipping. Acceptance: integration tests cover a chore with override that dispatches against the right URL, a chore with no resolution that surfaces an attention item and no execution row.

5. **CLI: `--repo` on create / update / list verbs** — chore + task + project. Empty-string clears; URL canonicalisation reuses the existing helper. Acceptance: `--help` covers the flag; integration test covers create-with-override → list-with-filter → update-to-clear → list-without-filter.

6. **CLI: prompt-text parser (`cli/src/repo_resolution.rs`)** — known-repo set query + tokeniser. Acceptance: unit tests cover the inference order; integration test covers `boss chore create` with a prompt that names a known repo, with a prompt that doesn't, and with `--no-input` and no resolution.

7. **CLI: `boss <kind> show` Repo line** — renders the resolved URL with provenance ("override on this work item" / "inherited from product `<slug>`" / "(none — work item cannot dispatch)"). Acceptance: golden-output test for each provenance case.

8. **Engine: cold-repo attention item (Q6)** — on first `ensure_repo` for a URL not previously seen, query `cube repo list --json` and raise an advisory attention item if cube's pool config matches its defaults. Dedup by URL. Acceptance: integration test runs a fake `cube repo list` returning a default-config pool, asserts the attention item is created exactly once.

9. **macOS: Product create form** — repo field optional with helper text. Acceptance: snapshot tests; visual review.

10. **macOS: Work-item create form** — repo field hidden when product has default (with override disclosure), shown when not (with optional "set as product default" checkbox). Recent-repos picker. Acceptance: snapshot tests; UI interaction test.

11. **macOS: kanban card repo chip** — chip in card header when product is multi-repo or any card overrides. Single-repo collapses to a column-header chip. Acceptance: snapshot tests for both modes.

12. **macOS: work-item detail Repo row** — displays resolved URL with provenance and a "Change…" affordance opening the picker. Acceptance: UI interaction test changes the override and observes the chip update.

13. **(Optional follow-up) `boss admin lint-repo-resolution`** — scan every non-deleted work item, report rows that resolve to `None`. v1 advisory; could become part of a wider repo-health audit.

14. **(Optional follow-up) Branch / subpath on the override** — extend `tasks.repo_remote_url` companions if a real use case emerges (e.g. a chore that needs to dispatch against a specific long-lived branch). Schema only, no behaviour change to v1.

15. **(Optional follow-up) Cross-product references** — separate design doc tracked under the same parent project (`proj_18a2bbe20fc03718_8`). The work-dependencies design already files this as out of v1 scope.

---

## Out of Scope

- Multi-repo *within* a single work item (one chore touching two repos in one run).
- Cross-product references (work item in product A pointing at work item in product B).
- LLM-based prompt-text parsing for repo inference.
- A separate `repos` registry table.
- Project-level repo override (Q2's option β / γ).
- Per-work-item branch / subpath / commit-pin overrides.
- Auto-detection of upstream repo rename / move / archive.
- Workspace-pool sizing automation across multi-repo products.
