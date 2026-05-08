# Boss: Design-Producing Work Items

## Problem

Boss's work-item taxonomy assumes the deliverable of every task is a *merged PR*. `task` and `chore` rows carry a `pr_url` column (`protocol/src/types.rs:46`), the kanban surfaces `in_review` by reading that column, and `PrDetector` (`engine/src/completion.rs:109`) is the bridge that flips a row to `in_review` when a worker stops with a pushed branch. Project tasks (`kind = "project_task"`) inherit the same shape.

That shape is a poor fit for the work I do most often when starting a new feature: writing a design document. My current pattern is to file the design as a regular `chore`, point the worker at `tools/boss/docs/designs/<slug>.md`, and accept that the worker is going to land the doc by opening a docs-only PR — `task_18ad650912a35110_16` (PR #234, work-dependencies) and the just-merged auto-rebase design (PR #249) are both examples. The PR adds nothing: there is no review on the design doc itself (the doc is reviewed by me clicking around the eventual implementation chores), the merge is a rubber-stamp, and the project CLAUDE.md already says *"Documentation-only changes ... should be pushed directly to `main` instead of opening a PR."* The PR is purely the engine's mechanism for transitioning the chore through `in_review → done`.

Worse, the *real* deliverable of a design chore — the pile of follow-up implementation tasks the doc enumerates — is hand-typed by me into `boss task create` calls after the doc lands. Every design doc in `tools/boss/docs/designs/` ends with a "Follow-up Implementation Chores" list precisely because that list is the load-bearing payoff and there is currently no automation that turns it into work-item rows. So the design author writes the work plan, I read it, I retype it, and the model has burned a chore on a docs-only PR in between.

This doc proposes a first-class **design-producing** work-item kind: one whose deliverable is a markdown document at a configured location, whose `in_review` surface links to the *file* (not a PR), whose review action is "approve the doc" rather than "merge a PR," and whose approval triggers the engine to materialise the projects and follow-up tasks the doc enumerates. The PR detour goes away; the manual list-retyping goes away; the kanban gains a new card type whose visual affordance matches what the work actually is.

## Goals

- A new work-item kind whose lifecycle terminates on **approval of a markdown document**, not on a merged PR.
- Doc location is per-product configurable, with a sensible default (the same-repo `tools/boss/docs/designs/` convention this repo uses today).
- Worker writes the doc and pushes it (commit, no PR) to a configured branch (default `main`); engine detects the doc and flips the work item to `in_review` with the file path / URL as the artifact reference.
- macOS app renders the doc in a side window with GitHub-flavoured Markdown styling and shows a single **Approve** button.
- On approval, the engine reads a worker-emitted manifest sitting alongside the doc and creates the projects and tasks the design called out — atomically, idempotently, with a clear undo path.
- Every existing piece of work-item plumbing that is not specific to PRs (the kanban, dependency edges, attention items, transcripts, executions) keeps working unchanged.
- Migration is a no-op for existing `task` / `chore` / `project_task` rows.

## Non-Goals

- **Inline / Google-Docs-style commenting on the rendered doc.** Single Approve button only in v1. Threaded review comes via `proj_18a2bb78815be670_3` (the doc-collab project), which already plans for this surface and explicitly subsumes future review tooling.
- **Cross-product / cross-repo doc location resolution.** v1 handles two cases: same-repo docs (the `mono` convention) and per-product separate-repo docs. Spanning organisations or fan-out across multiple repos for one design is deferred (filed against the same cross-product project as the dependencies design).
- **Edit-existing-doc diffs as a first-class review surface.** v1 renders the doc post-edit; if the user wants to see what changed, they read the commit diff in the underlying repo. Inline diff rendering inside the renderer is on the post-v1 list.
- **Approval of partial doc updates.** Each `in_review` cycle approves the *whole* doc as it currently stands; we don't track "approved up to revision X."
- **Round-tripping the manifest back into the doc.** Once a manifest is produced and approved, edits to the doc do not retroactively edit the spawned tasks. Re-running the design task to produce a v2 doc is the supported path; the engine treats it as a separate `in_review` cycle.
- **Replacing PRs for code-modifying work.** Design tasks are docs-only; a follow-up implementation chore that *modifies code* still produces a PR. The two kinds coexist, with the design task *upstream* of the implementation chores it spawns.

## Naming

- New work-item kind: **`design`** (`tasks.kind = 'design'`). Same parent table; new value in the `kind` discriminator. We considered `design_task` but it adds a redundant suffix — `task` is already the row's noun in `tasks.kind`, the discriminator is the modifier. CLI noun: `boss design ...`.
- Output artifact: **the doc**, identified by a `doc_ref`. A `doc_ref` is `(repo_remote_url, branch, file_path)` — branch is normally `main`, but per-product config can change it.
- Worker manifest: **`tasks.json`** (or `<slug>.tasks.json`) sitting alongside the doc in the repo. Contents are the structured work-item creates the engine should perform on approval.
- Review surface: the **renderer window**. The kanban card's "open" affordance for a `design` row in `in_review` opens the renderer; PR rows continue to open the GitHub PR URL.
- The action: **Approve**. The RPC is `ApproveDesign` (singular noun). Auth is the same actor model as today (`last_status_actor`).

---

## Design Question 1 — Schema and Kind

### Options

- **(a) New `kind = 'design'` value.** Same `tasks` table, same row shape, new discriminator value alongside `task`, `chore`, `project_task`. CLI gains `boss design ...`. Code that currently filters / matches on `kind` learns the new value.
- **(b) Boolean flag `is_design` on existing `kind`.** A `chore` row gains `is_design = 1` to opt in. Existing `kind` discriminator unchanged.
- **(c) New table entirely (`designs`).** Foreign-keyed parallel to `tasks`. Lifecycle column overlaps with `tasks.status`.
- **(d) Reuse `kind = 'chore'`, store the design-ness in `metadata` JSON.** Cheapest schema, most ambiguity in code.

### Discussion

(a) is the natural extension of the existing model. `kind` is already the "what shape of work is this" discriminator: `task` (project leaf), `chore` (free-floating), `project_task` (parent of tasks). Adding `design` gives us a fourth shape with the same parent table, the same kanban swimlanes, the same execution / transcript plumbing. CLI nouns line up: `boss task ...`, `boss chore ...`, `boss design ...` is the obvious progression.

(b) sounds cheaper but is the same trap as the rejected `(B) is_system flag` in `auto-rebase-stacked-prs.md` Q3: a parallel boolean doubles the discriminator role and means every list / filter / RPC has to remember to AND in the new bit. The kanban card needs to render a design-shaped affordance differently — that decision is now spread across two columns instead of one.

(c) duplicates infrastructure. Designs need executions (the worker that produces them), runs, transcripts, attention items, dependency edges. All of those today key on `tasks.id`. A separate table means either every join becomes a UNION ALL, or designs can't have those things — neither is good.

(d) is the worst because the type signature of "what is this row" depends on a JSON blob. Engine code that wants to know "is this a design?" has to parse JSON on every check. Hard pass.

### Recommendation

**Pick (a).** Add `'design'` to the set of legal `tasks.kind` values. Storage stays in `tasks`; lifecycle stays in `tasks.status`. The CLI gains a `boss design` noun whose verbs mirror `boss task` (`create`, `list`, `show`, `update`, `delete`) plus a few design-specific ones (`approve`, `revise`).

#### What "design" means in code

A row with `kind = 'design'` differs from a `chore` in three places:

1. **The auto-dispatcher's "ready to spawn" check** — same as a chore.
2. **The completion / `in_review` transition** is driven by **`DesignDetector`**, not `PrDetector` (Q5). The work item flips to `in_review` when the worker pushes the doc and prints its `doc_ref` on its final line, *not* when it opens a PR.
3. **The `done` transition** is driven by **`ApproveDesign`** (Q7), not by GitHub merge events. There is no `pr_url` for design rows; the column is permitted to be `NULL` for `kind = 'design'` and the engine never reads it for these rows. (We don't drop the column — schema gymnastics for a per-kind difference aren't worth it; just don't populate it. See Q2.)

Everything else — the kanban swimlanes, dependency edges, transcripts, attention items, executions, runs — works unchanged. A `design` row participates in `work_item_dependencies` like any other; you can have a chore that's blocked on a design (the implementation can't start until the design is approved), and the auto-unblock path from the dependencies design (`work-dependencies.md`) handles it for free.

#### One-deliverable-per-task philosophy

The user has stated elsewhere that one PR per task is the philosophy. Designs naturally fit: one doc per design row. If a design naturally splits into two docs (a high-level + a detailed appendix), that's two design rows linked by a dependency edge, not one design with two docs. This keeps the row → artifact mapping 1:1, which keeps the renderer's "open" affordance unambiguous.

---

## Design Question 2 — Output Artifact Tracking

### Options

- **(A) Reuse `tasks.pr_url`** to store the `doc_ref` URL. Save the column, change the meaning per `kind`.
- **(B) New column `doc_ref`** that coexists with `pr_url`. For `kind = 'design'`, `pr_url` is always NULL and `doc_ref` carries the artifact; for other kinds it's the reverse.
- **(C) Single column `artifact_url`** that replaces `pr_url`. Migration renames; per-`kind` semantics determine what the URL points to.
- **(D) New table `design_artifacts`** keyed by `task_id` carrying `(repo_remote_url, branch, file_path, manifest_path, manifest_sha)`.

### Discussion

(A) is tempting and the engine code that surfaces a card's artifact already reads one column. But `pr_url` semantically *means* "PR URL" — every consumer (the macOS app's `Models.swift:825`, the kanban's open-affordance handler) has hardcoded GitHub-PR assumptions: parsing `https://github.com/.../pull/N`, fetching merge state via `gh pr view`, etc. Bending those callers to also handle "this might be a tree URL to a markdown file" is a wide-blast-radius change for a string that's just a discriminated union with extra steps.

(B) is honest: `pr_url` keeps its meaning, `doc_ref` is the design-specific one, and per-`kind` gating in the consumer code says "this row's artifact is over here." The cost is one extra nullable column on `tasks`.

(C) renames a heavily-used column for one new kind. The migration touches every `*-list` / RPC / app shape. The semantic confusion ("artifact_url" — for a chore, it's a PR; for a design, a doc; for a project_task, what?) is exactly what (A) suffers from. Pass.

(D) is overengineering for v1. Designs do need *some* extra metadata beyond the URL — at least the manifest path and the manifest sha for idempotency on approval (Q8) — but a separate table for one kind isn't justified until those grow. JSON-typed `metadata` on `tasks` is plenty.

### Recommendation

**Pick (B), with the extra metadata in `tasks.metadata`** (the existing JSON column, see `tasks.metadata` in the schema) rather than its own table.

```sql
ALTER TABLE tasks ADD COLUMN doc_ref TEXT;       -- e.g. 'tools/boss/docs/designs/foo.md'
                                                 -- for in-repo docs; full https://… URL
                                                 -- for separate-repo docs.
```

`tasks.metadata` (JSON) gains for `kind = 'design'`:

```json
{
  "design": {
    "doc_ref": "tools/boss/docs/designs/design-producing-tasks.md",
    "doc_branch": "main",
    "doc_repo_remote_url": "https://github.com/brianduff/mono.git",
    "manifest_path": "tools/boss/docs/designs/design-producing-tasks.tasks.json",
    "manifest_sha": "abc123…",   // sha256 of the manifest file at the moment
                                  // the worker last reported it; used for
                                  // idempotency on approval (Q8).
    "approved_at": "2026-05-09T11:14:02Z",   // null until Approve
    "approved_by": "human"                    // 'human' or an actor id
  }
}
```

The top-level `doc_ref` column duplicates one field from this JSON. Justification: SQL filtering by `doc_ref` is common (engine wants to deduplicate designs that share a doc, kanban wants to render the artifact link without parsing JSON), and the cost of one denormalised TEXT column is minimal. The JSON carries everything else.

For non-design rows, both `doc_ref` and `metadata.design` are NULL.

`pr_url` is unchanged. Nothing about today's PR-driven kinds breaks.

---

## Design Question 3 — Per-Environment Doc Location

### The two real cases

- **Home (this repo)**: docs live alongside the code at `tools/boss/docs/designs/<slug>.md`. Same repo, same branch, same checkout. The worker that produces the doc is leasing the same workspace where the design `chore` lives.
- **Work**: docs live in a separate context / wiki-style repo. The code repo and the doc repo are distinct. The worker leasing the code workspace doesn't have the doc repo cloned.

### Options

- **(α) Per-product `docs_location` column.** Stores either a path-within-repo (`tools/boss/docs/designs/`) or a separate-repo URL (`https://github.com/myorg/docs.git`).
- **(β) Per-task explicit field.** Each design row sets its own location; no product default.
- **(γ) User-level standing instruction.** Lives in `~/.claude/CLAUDE.md` or the project CLAUDE.md and is threaded through the spawn prompt. The engine doesn't model it as data.
- **(δ) Hybrid.** Per-product default with per-task override; standing-instruction as a hint to the worker but not the canonical source.

### Discussion

(γ) is the user's current de-facto setup — the `mono` repo's CLAUDE.md says docs go to `tools/boss/docs/designs/`, and at work the standing instruction would point at the docs repo. Making it the canonical source would mean the engine reads CLAUDE.md, which is fragile and untyped — every product's CLAUDE.md format would have to satisfy a parser.

(α) makes the location data the engine controls. The trade-off is a new column on `products` and a CLI verb to set it.

(β) on its own is too much per-task ceremony — the location is almost always the same per product.

(δ) is the right shape: data on `products` for the canonical location; per-task override for the rare "this design lives somewhere weird" case; standing-instruction is downstream advice for the worker (still useful so the worker knows the *style* expectations) but not the lookup mechanism.

### Recommendation

**Pick (δ).** Concrete shape:

```sql
ALTER TABLE products ADD COLUMN docs_location TEXT;
-- e.g. 'tools/boss/docs/designs/'  (in-repo; relative path)
-- e.g. 'https://github.com/myorg/docs.git#docs/designs/'  (separate repo + path)
ALTER TABLE products ADD COLUMN docs_branch   TEXT NOT NULL DEFAULT 'main';
```

A `design` row inherits `(docs_location, docs_branch)` from its product unless it carries an explicit override in `metadata.design.doc_repo_remote_url` / `doc_branch`. The slug for the doc filename comes from the design row's `slug` (existing column on `tasks`), so a row with `slug = 'design-producing-tasks'` and product-level `docs_location = 'tools/boss/docs/designs/'` resolves to `tools/boss/docs/designs/design-producing-tasks.md`.

#### Separate-repo case

When `docs_location` is a `https://…git#path/` form, the spawn prompt for the design task tells the worker:

> Your deliverable doc lives at `<docs_location>/<slug>.md` in `<docs_repo_remote_url>` on branch `<docs_branch>`. The cube workspace does not contain this repo by default. Use `cube workspace lease --repo <docs_repo_remote_url>` (or your normal lease command if cube has the secondary repo registered) to obtain a checkout. Commit and push the doc directly to `<docs_branch>` — no PR.

The cube tooling already supports multi-repo workspaces, so this is a configuration concern rather than a code change in the engine. (At-work setup is the user's responsibility; the engine just threads the location into the prompt.)

#### Default

If `docs_location` is NULL, a sensible default is `tools/boss/docs/designs/` relative to the product's primary repo. This matches the `mono` convention and "just works" for repos that already follow this layout.

---

## Design Question 4 — Doc-Direct-to-Main Commit Policy

### What today's worker does

A standard chore worker runs:

1. `jj git fetch && jj new main`
2. edits + `jj describe -m "..."`
3. `jj git push -b <bookmark>` — creates a feature branch on the remote.
4. `gh pr create` — opens a PR.
5. Stop with the PR URL on its final line.

For a design task, steps 3–5 must change. The deliverable is a commit on `main` (or whatever `docs_branch` is), not a PR.

### Options

- **(i) Spawn-prompt tells the worker.** Add explicit instructions to the `design`-kind spawn prompt: *"Commit the doc with `jj describe -m '...'`; push directly to `<docs_branch>` with `jj git push --bookmark <docs_branch>` (or `git push origin HEAD:<docs_branch>`). Do not run `gh pr create`. On the final line of your last response, print `DOC_REF: <repo_remote_url>:<branch>:<file_path>`."*
- **(ii) New tool affordance.** A custom tool (e.g. `boss design publish`) that wraps the right git/jj sequence and prints the structured DOC_REF line. The worker calls the tool instead of raw shell.
- **(iii) Engine post-processing.** The worker creates a feature branch like a normal chore. The engine sees the design task, fast-forwards `<docs_branch>` to the worker's feature branch tip, deletes the feature branch, and updates the row. Worker doesn't need to know about the direct-to-main policy.
- **(iv) Per-environment branch protection check.** The engine probes the remote: if `<docs_branch>` is protected against direct pushes, fall back to PR; else direct push.

### Discussion

(i) is the cheapest and most transparent. The worker already follows the spawn prompt for everything else; one more block of instructions costs nothing and is debuggable (read the prompt, see what the worker was told). The only risk is "worker still opens a PR because it's habit" — addressed by Q5's `DesignDetector` which simply doesn't recognise a PR as the deliverable, so a worker that opens one won't progress the row. The worker will get probed by the coordinator and corrected.

(ii) is nicer DX once we have many design tasks, but premature for v1. Same outcome as (i) plus one more dev tool to maintain.

(iii) is clever but couples the engine to the worker's `jj` state and runs `jj` operations inside engine code outside a leased workspace — neither is desirable and we just spent `auto-rebase-stacked-prs.md` Q5 deciding *against* engine-side `jj` for non-trivial cases. Pass.

(iv) is overengineered: branch protection is fine to detect, but the *policy* of "design = direct push" should be a deliberate per-product decision, not an emergent property of GitHub configuration. If `main` is push-protected, the right answer is "configure `docs_branch` to a different name, or unprotect," not "fall back silently to the PR path."

### Recommendation

**Pick (i).** The `design`-kind spawn prompt is a fixed template (built in `engine/src/spawn_flow.rs`) with a known set of variables (`docs_location`, `docs_branch`, `slug`, `manifest_path`). Render the direct-push instructions verbatim. Worker takes it from there.

Concretely, the spawn-prompt addendum for `kind = 'design'`:

```
## Your deliverable is a markdown design document, not a PR.

Doc location: <repo_remote_url> @ <docs_branch> : <docs_location>/<slug>.md
Manifest:     <repo_remote_url> @ <docs_branch> : <docs_location>/<slug>.tasks.json

Steps:
1. `jj git fetch`
2. `jj new <docs_branch>` (or `jj edit <docs_branch>` if continuing).
3. Write the doc at `<docs_location>/<slug>.md`. Match the style of
   peers in the same directory (Problem / Goals / Non-Goals / Naming /
   Design Question N sections / Recommendation summary / Schema and
   Wire / Risks / Follow-up Implementation Chores).
4. Write the manifest at `<docs_location>/<slug>.tasks.json` — a JSON
   document describing the projects and tasks that should be created
   when the doc is approved. See the schema below.
5. `jj describe -m "design(<product-slug>): <doc title>"`.
6. `jj git push --bookmark <docs_branch>` (or `git push origin
   HEAD:<docs_branch>`). Do NOT run `gh pr create`.
7. On the final line of your final response, print exactly:
   DOC_REF: <repo_remote_url>|<docs_branch>|<docs_location>/<slug>.md
```

The `DOC_REF:` sentinel is parsed by `DesignDetector` (Q5) — same shape as `PrDetector`'s pattern but with a different prefix.

#### Why a sentinel rather than git-side detection

We could try to detect "the worker pushed a `.md` file to `<docs_branch>`" by polling `git log` on the doc location. But:
- The branch may already have many commits the worker isn't responsible for (it's `main`!), so we'd need to scope to a sha range, which means knowing the worker's start point.
- Multiple workers may push to `main` between detector runs.
- The worker's commit could land but the manifest write fail; sentinel says "I'm done with both."

The sentinel is one printf and unambiguous. It's the same trick `PrDetector` uses today; reuse the pattern.

---

## Design Question 5 — In-Review Detection (`DesignDetector`)

### What `PrDetector` does today

`engine/src/completion.rs:109` defines a `PrDetector` trait. The on-Stop path calls it with the worker's transcript and bookmark; the implementation greps the last line for a GitHub PR URL, then hits `gh pr view` to confirm `state` and capture `mergedAt`. On `Created` or `Merged`, the engine flips the row's status (`in_review` or `done`) and stores the URL in `tasks.pr_url`.

### Options

- **(I) Reuse `PrDetector` with a "design" output type.** Trait gains a third return variant; the matcher learns the `DOC_REF:` sentinel.
- **(II) New `DesignDetector` trait, parallel to `PrDetector`.** Same on-Stop call site dispatches based on `task.kind`.
- **(III) Generic `ArtifactDetector` trait.** One trait, multiple matchers (PR, doc, future kinds). Strategy pattern.

### Discussion

(III) is the right shape long-term and matches the auto-rebase doc's preference for not premature-abstracting. Once we have two artifact types we have a pattern; right now we have two and only two, and the `PrDetector` interface is already shaped around git refs and `gh pr view`, which doesn't generalise to a doc reference. Trying to retrofit one trait covers both is going to leak abstractions.

(I) overloads `PrDetector` with a non-PR concept. The trait's name lies, the variants multiply, and downstream code has to switch on the variant anyway.

(II) is parallel and clear. `DesignDetector::detect_from_stop(&self, task: &Task, transcript: &Transcript) -> Option<DocRef>`. The on-Stop path in `completion.rs` does:

```rust
match task.kind.as_str() {
    "design" => design_detector.detect(...).map(StopOutcome::DesignReady),
    _        => pr_detector.detect(...).map(StopOutcome::PrCreated /* etc. */),
}
```

### Recommendation

**Pick (II) for v1; revisit (III) when a third artifact kind arrives.** The two detectors live side by side in `engine/src/completion.rs` (or split out to `engine/src/design_detector.rs` once the file gets unwieldy).

#### `DesignDetector` contract

1. **Read the worker's transcript final lines.** Same source `PrDetector` uses today.
2. **Match the sentinel** `^DOC_REF:\s*(?P<repo>\S+?)\|(?P<branch>\S+?)\|(?P<path>.+?)$` on any of the last 5 lines. (Five, not just the last, because tools sometimes append a trailing blank or status line.)
3. **Validate the doc exists** by issuing `gh api /repos/<owner>/<repo>/contents/<path>?ref=<branch>` (or `git cat-file -p <branch>:<path>` against a local clone if the doc lives in the same repo as the workspace). On 404, treat as detection failure — the worker claimed they pushed but the doc isn't there.
4. **Validate the manifest exists** at the conventional path (`<doc_path_minus_md>.tasks.json`). On 404, the design is detected as `in_review` but with `metadata.design.manifest_missing = true`; on approval, the engine spawns a *manifest-extractor* worker (Q8) instead of reading a non-existent file.
5. **Capture the manifest's sha** (`gh api /contents/...` returns `sha`) and stash it in `metadata.design.manifest_sha`.
6. **Return** `Some(DocRef { repo_remote_url, branch, file_path, manifest_present, manifest_sha })`.

The on-Stop handler then writes:

```sql
UPDATE tasks
   SET status = 'in_review',
       doc_ref = :file_path,
       metadata = json_set(metadata, '$.design.doc_repo_remote_url', :repo,
                                     '$.design.doc_branch', :branch,
                                     '$.design.manifest_path', :manifest_path,
                                     '$.design.manifest_sha', :manifest_sha)
 WHERE id = :task_id;
```

… and broadcasts `work_item_changed` like any other status flip.

#### What if the worker stops without printing `DOC_REF:`?

Same handling as a worker that stops without pushing a PR today: the row stays in `active`. The completion path emits a `WorkAttentionItem` ("Design task stopped without DOC_REF sentinel — did the worker forget to push?") and the user resumes / probes. No silent regression.

#### Out-of-band doc updates

A user (or another worker) pushes an edit to the doc directly without going through a design task. `DesignDetector` doesn't fire — there's no Stop event. The doc just gets newer; the design row's row stays `in_review` with a stale `manifest_sha`. On approval, the engine re-fetches the manifest (sha check catches the drift), refuses to apply if the manifest changed substantively (Q8 / Q12), and asks the user to re-trigger detection. Acceptable for v1.

---

## Design Question 6 — Markdown Renderer

### Options

- **(P) `WKWebView` + bundled `cmark-gfm` build step.** The doc is rendered to HTML server-side (Rust, via `pulldown-cmark` or `comrak` already in the workspace), then displayed in a webview with a CSS file matching the kanban's visual language.
- **(Q) SwiftUI `AttributedString.init(markdown:)`.** Native, no dependencies, but supports only inline markdown — no headings, lists, tables, code blocks, fenced blocks. Useless for a design doc.
- **(R) `Textual` (already in `Package.swift`, `gonzalezreal/textual`).** A SwiftUI-native CommonMark renderer. Already a dependency; already used in `ContentView.swift:3`.
- **(S) Shell out to `pandoc` for HTML, render in `WKWebView`.** Rust-side HTML generation; webview display. Requires `pandoc` in PATH.

### Discussion

(Q) is out — it can't render the structures these design docs use (tables, fenced code blocks, multi-level headings).

(S) introduces a dependency on `pandoc` being installed on every dev / user machine. The macOS app today is self-contained; bundling pandoc or asking users to `brew install pandoc` is not the right ergonomic.

(P) is the most flexible but builds new infrastructure: a `comrak` (or `pulldown-cmark` with `MarkdownExtensions`) pipeline in the engine, an HTML+CSS template, a webview wrapper. It also drags in a "rendered HTML" step that obscures debugging — the user sees HTML, not the raw markdown they wrote.

(R) is the simplest *correct* option. `Textual` already renders CommonMark with SwiftUI views, supports headings, lists, tables, code blocks, and inline styles. We already have it as a dependency for the chat panel. Reusing it for the design renderer is one new SwiftUI view (`DesignRendererView`) that takes a string and a few buttons.

The honest gap of (R) is **GitHub-specific extensions** (autolinks for issue/PR references, task-list checkboxes inside fenced blocks, footnotes). Textual's CommonMark coverage is strong but not 100% GFM-equivalent. For the docs we write today (the existing `tools/boss/docs/designs/*.md` files), the visible gaps are: no PR-number autolinks, no task-list rendering. Both are tolerable for v1; both are the kind of thing Textual will likely add over its lifetime, or we contribute upstream.

### Recommendation

**Pick (R) — `Textual`.** Build a `DesignRendererView` SwiftUI window that:

- Loads the doc by `(repo_remote_url, branch, file_path)` via the existing engine RPC surface (a new `GetDesignDoc` RPC that fetches the file content, see Q7).
- Renders with `Textual.MarkdownView(document)`.
- Wraps in a `ScrollView` with a fixed-width content column (~720pt) so long lines don't sprawl across an ultrawide.
- Header bar carries: doc title (from H1), the design row's name in dimmer text, a status chip (`in_review` / `approved`), the **Approve** button, and an "Open in Editor" button (opens the file in the user's `$EDITOR` or VS Code via the existing repo path resolution).
- Footer carries the manifest summary: *"On approval, this will create 1 project, 8 tasks. [Show details]"* — clicking expands a tree of the manifest contents (Q7 / Q8).

Renderer-window lifecycle: opened via the kanban card's chevron / "open" affordance; multiple windows allowed (different designs side-by-side). State (scroll position, expansion) is per-window and not persisted — these are review aids, not editors.

#### Future expansion

When we want richer rendering — diff view (Q9), inline comments (out of scope) — the renderer is the place to add those affordances. `WKWebView` becomes a real option once we want HTML-grade fidelity (e.g. Mermaid diagrams). v1 doesn't need it; document the upgrade path in the Risks section.

#### Where the rendered file content comes from

The renderer doesn't talk to GitHub directly. The engine RPC `GetDesignDoc(task_id)` resolves the doc content via:

- If the doc lives in the product's primary repo and a leased cube workspace exists, read from the workspace filesystem — fastest, no network.
- Otherwise, `gh api /repos/<owner>/<repo>/contents/<path>?ref=<branch>` — falls back to a network round-trip.

The engine handles caching with the manifest-sha as the cache key.

---

## Design Question 7 — Approval Affordance

### Where the button lives

- **The renderer window** — primary surface. Big, obvious **Approve** button in the header, only enabled when the row is in `in_review`.
- **The kanban card** — secondary surface. A small "Approve" affordance on the card's hover menu or right-click. Convenience for "I've already read it; just approve."
- **The CLI** — completeness. `boss design approve <selector>`.

All three call the same engine RPC.

### The RPC

```rust
// in tools/boss/protocol/src/wire.rs
ApproveDesign {
    request_id: String,
    input: ApproveDesignInput,
}

// in types.rs
pub struct ApproveDesignInput {
    pub task_id: String,
    pub manifest_sha: Option<String>,    // optional client-side check
                                         // — see Q8 idempotency
    pub revoke: bool,                    // false = approve, true = un-approve
                                         // (see Q12 — recovery from
                                         // mistaken approval).
}

pub struct ApproveDesignOutput {
    pub task: Task,
    pub created_projects: Vec<Project>,
    pub created_tasks: Vec<Task>,
    pub skipped_existing: Vec<String>,   // ids that were already present
}
```

### Authorisation

Single-user repo today. `last_status_actor` is set to the calling actor (`'human'` from CLI / app). Future multi-user setups can layer authz on top of this RPC; v1 trusts the caller.

### Recommendation

**Approve button on the renderer window is the primary surface.** Put the same affordance on the kanban card hover menu (one menu item, no extra confirmation dialog beyond the standard `last_status_actor` audit trail). CLI verb `boss design approve <selector>` rounds it out for headless use.

The button is only enabled when:
- `task.kind == 'design'`
- `task.status == 'in_review'`
- `metadata.design.doc_ref` is set

Disabled tooltip text in any other state explains why.

#### Confirmation friction

Should approving pop a confirmation dialog *("This will create 1 project and 8 tasks. Approve?")*? Mild yes — but I'd render the confirmation as the renderer-window's footer (Q6), not a modal alert. The user reads the doc; they see "On approval, this will create 1 project, 8 tasks. [Show details]" right there; the Approve button doubles as the confirmation. One click, one consent.

Modal alerts are friction the user has already opted into by clicking Approve on the renderer; a second confirmation buys nothing.

---

## Design Question 8 — Post-Approval Automation

### Options

- **(α) Convention-driven parser.** Engine reads the doc itself, finds the `## Follow-up Implementation Chores` section, parses it for numbered list items, creates a chore per item. Brittle but cheap.
- **(β) Worker-driven extractor.** On approval, engine spawns a *new* short-lived worker whose job is to read the doc and emit a structured manifest as JSON; engine then applies the manifest. Slow, LLM cost, but robust to varied doc shapes.
- **(γ) Worker-emitted manifest.** The design task's worker, while writing the doc, *also* writes a `<slug>.tasks.json` file alongside it. Engine reads the JSON on approval. Predictable, schema-validated, no extra LLM round-trip.
- **(δ) Hybrid: γ as primary, β as fallback.** If the worker emitted the manifest, use it; if not, spawn an extractor worker on approval.

### Discussion

(α) runs into the brittleness wall the auto-rebase doc Q11 conflict-diagnosis section also flagged: prose is varied. Five docs in `tools/boss/docs/designs/` already use slightly different headings ("Follow-up Implementation Chores" vs "Implementation Plan" vs none at all). Parser tweaks lag doc reality. Not safe.

(β) makes approval slow — a fresh Claude turn to extract the manifest costs seconds at minimum and dollars at scale, plus the worker can fail (network, model error) at the moment the user just clicked Approve. Bad UX.

(γ) is the cleanest: the design worker is *already* writing the doc, *already* enumerating the tasks at the bottom of it. Asking the worker to also emit a JSON sibling is a small additional step in the same turn, and the engine gets a deterministic, schema-validated input on approval. Schema is small enough to fit in the spawn prompt.

(δ) hedges: if for some reason the worker didn't emit a manifest (older design, manual doc), fall back to an extractor. The fallback is never fast, but it's also a rare case.

### Recommendation

**Pick (δ) — γ as the standard path, β as fallback for missing manifests.** v1 builds γ; β arrives as a follow-up chore (so users can adopt the new affordance even on docs that pre-date the manifest convention).

#### Manifest schema

```json
{
  "schema_version": 1,
  "design_task_id": "task_18ad…",
  "created_for_doc_ref": "tools/boss/docs/designs/design-producing-tasks.md",
  "projects": [
    {
      "external_id": "design-producing-tasks-impl",
      "name": "Design-producing tasks: implementation",
      "slug": "design-producing-tasks-impl",
      "description": "Implements the design described in design-producing-tasks.md.",
      "goal": "Land a working `design` work-item kind end-to-end."
    }
  ],
  "tasks": [
    {
      "external_id": "schema-migration",
      "kind": "chore",
      "name": "Schema + migration: add `design` to tasks.kind, doc_ref, products.docs_location",
      "description": "...",
      "project_external_id": "design-producing-tasks-impl",
      "ordinal": 1,
      "depends_on_external_ids": []
    },
    {
      "external_id": "design-detector",
      "kind": "chore",
      "name": "engine: DesignDetector + on-Stop wiring",
      "description": "...",
      "project_external_id": "design-producing-tasks-impl",
      "ordinal": 2,
      "depends_on_external_ids": ["schema-migration"]
    }
  ]
}
```

`external_id` is a worker-supplied identifier scoped to the manifest — it is *not* the engine task id. The engine resolves dependencies by `external_id` at apply time (a chore that depends on `external_id = "schema-migration"` gets a `work_item_dependencies` row pointing at whichever real task id `schema-migration` materialised as).

#### Apply algorithm

The engine's apply step (`work.rs::apply_design_manifest`) runs in a single transaction:

1. Validate the manifest against the schema (use `serde_json` + a `JsonSchema` derive or hand-rolled validator). On invalid → fail with a clear error; row stays `in_review`.
2. Topologically sort projects and tasks by `external_id` dependencies. Reject cycles.
3. For each project: check if a project with the same `slug` already exists in the same product. If yes, skip (record in `skipped_existing`). If no, `INSERT` and remember the new `id`.
4. For each task: same `slug` dedup. If new, `INSERT`; if existing, skip but still record the `external_id → id` mapping so dependency edges can be created.
5. For each task with `depends_on_external_ids`: `INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation)` for each edge.
6. Update `tasks.status = 'done'`, `metadata.design.approved_at`, `metadata.design.approved_by` on the design row.
7. `last_status_actor = 'human'` for the design row's flip; `last_status_actor = 'engine'` for the spawned rows (so the dependency-design auto-unblock is allowed to act on them).
8. Broadcast `work_item_changed` for the design row plus a batched `work_items_created` event for the new rows (the kanban can re-fetch in one round-trip).

All-or-nothing: any error in steps 2–7 rolls the transaction back; the design row stays `in_review` with an `WorkAttentionItem` describing what failed (Q12).

#### Idempotency

The apply step keys on `(slug, product_id)` for each created row. Re-running the apply (manual retry after a transient SQL error) is safe: existing rows are skipped, new rows are added. The `manifest_sha` recorded at detection time gives the engine a way to detect "manifest changed under us" — if the on-disk manifest's sha differs from `metadata.design.manifest_sha`, the engine refuses the apply and asks the user to re-detect (i.e. resume the design row from `in_review` with the new manifest). See Q12.

#### Why not auto-create dependency edges from `ordinal` only?

Because `ordinal` is a soft hint, not a constraint. The dependencies design (`work-dependencies.md`) deliberately separates the two; this design respects that separation. If the worker means "this task is gated on that one," they add an `external_id` to `depends_on_external_ids`; if they just mean "this is logically before that one in a list," they use `ordinal`.

---

## Design Question 9 — Edit-Existing-Doc Case

### Two sub-cases

- **Editing a previously-approved design doc.** A new design row reopens an existing doc to add a section / revise a recommendation. The `tasks.json` may add new follow-up tasks (already-spawned ones aren't recreated thanks to slug dedup, Q8). Approving the new row applies the manifest's *delta*.
- **Editing an in-flight design (the worker iterates on the doc before it's first approved).** The worker stops, prints `DOC_REF:`, the row goes to `in_review`. The user comments via probe ("nope, the renderer choice is wrong, retry"), the user moves the row back to `active`, the worker spawns again with the doc as starting state. New `DOC_REF:`, new `in_review`, new approve cycle. No special handling needed — the existing `active → in_review` cycle accommodates it.

### Options for the "what does in_review link to" question

- **(♠) The file (post-edit state).** The renderer just shows the current contents. User sees what they're approving.
- **(♣) The diff (pre vs post).** The renderer shows a diff view of what changed in this revision.
- **(♥) Both — file by default, diff on toggle.** Best of both worlds; more UI work.

### Discussion

(♣) on its own is bad: the user is approving the *resulting state*, not the diff. Approving "+ remove paragraph 5" is not the operation they want — they want to approve "the doc says X." Diff alone is a code-review mental model that doesn't fit here.

(♠) is the v1 ship. The user reads the (now-updated) doc and approves the whole thing.

(♥) is the right end state. Add a "Show changes since last approval" toggle in the renderer that swaps the view to a unified diff (using the previously-approved sha as the baseline). It's a small SwiftUI affordance once we have rendered diffs in any form.

### Recommendation

**Ship (♠) for v1, file (♥) as a follow-up.** The renderer always shows the file. For edited-existing docs, the renderer footer gains a small line: *"Last approved: 2026-04-22T11:14Z. Click to compare with current."* The compare action opens the file in `$EDITOR` against `git show <prev_sha>:<path>` — it's not in-app, but it's serviceable.

The previously-approved sha is stored: every time `ApproveDesign` succeeds, the engine snapshots `metadata.design.last_approved_sha`. Subsequent approvals can diff against it.

#### Manifest deltas on re-approval

When a design row is re-approved with a new doc / new manifest:

- New projects / tasks (slug not seen before) are created normally.
- Existing projects / tasks (slug already exists) are skipped — *not* updated. Re-approval is additive only.
- A task that was in the previous manifest but is *not* in the new one is left alone (not deleted). Cleaning up implementation tasks the design abandoned is a manual action the user takes.

This is conservative and reversible: re-approval can only add, never destroy.

---

## Design Question 10 — Bootstrap

### The chicken-and-egg

The current chore (this very work item) is *itself* a design task. There's no `design` kind yet. The current work item's `kind` is `chore`, and per project CLAUDE.md it'll be committed direct-to-main as a docs-only change (rather than merging the design via PR).

Once the feature ships, future design tasks use the new affordance; this work item retroactively becomes "the design that bootstrapped the design kind."

### Recommendation

Document the bootstrap path explicitly:

1. **This chore is a regular `chore` row.** It produces `tools/boss/docs/designs/design-producing-tasks.md` (this file) and is committed direct-to-main per the project CLAUDE.md doc rule.
2. **The implementation chores enumerated in this doc's Follow-up Implementation Chores section** are also regular chores until the feature lands. They produce code PRs in the normal way.
3. **Once the feature is shipped end-to-end** (i.e. once Approve UI works, post-approval manifest application works), the *next* design task uses `kind = 'design'` and the new flow.
4. **A retroactive cleanup chore**, low priority, converts this row's `kind` from `chore` to `design`, sets `doc_ref = 'tools/boss/docs/designs/design-producing-tasks.md'`, populates `metadata.design.*` from the existing doc, and closes the row as `done`. The conversion is a one-shot script (`tools/boss/cli/src/migrations/convert_design_chore.rs`?) or a manual `boss design convert <task_id> --doc tools/boss/docs/designs/...`. Either is fine; the row count of pre-design-kind designs is tiny.
5. **Self-referential follow-up tasks.** This doc's Follow-up Implementation Chores list does *not* yet benefit from the manifest-driven materialisation it describes. Those chores have to be enqueued by hand once the design is approved (i.e. once the user reads this doc and is happy with it). After the feature lands, future design docs benefit.

The retroactive-conversion chore is pure tidiness: nothing breaks if it's never done. Listing it in the Follow-up section is the right place.

---

## Design Question 11 — Relationship to Other Open Work

### `boss task bind-pr`

A separately-queued thread proposes a `boss task bind-pr` verb that *binds* an externally-created PR to an existing task row, addressing the orphan-PR / one-PR-per-task threads. Design tasks **don't have PRs**, so:

- `boss task bind-pr` is irrelevant for `kind = 'design'` rows. The verb's authorisation should reject `kind = 'design'` with a clear error.
- The orphan-PR detection thread is unaffected: the engine still scans for PRs not associated with any task; design tasks are excluded from that scan because they explicitly never have PRs.

### One-PR-per-task philosophy

Reaffirmed. Design tasks have *one doc per row*, which is the same shape (1:1 row→artifact). Re-using the same doc for two design rows is forbidden by Q8's `(slug, product_id)` uniqueness on the design row's slug — same as the PR-side rule.

### `proj_18a2bb78815be670_3` (doc-collab project)

The doc-collab project plans Google-Docs-style commenting on rendered docs. Design tasks are the natural first consumer of that surface — they're the docs we'd most want comments on. v1 ships with no commenting; the doc-collab project, when it lands, layers commenting onto the renderer window built here.

### Dependency edges (`work-dependencies.md`)

Already integrated. A `chore` that implements part of a design can be created with a `depends_on` edge to the `design` row — it sits in `blocked` until the design is approved (which flips the design to `done` and auto-unblocks the dependent via the dependencies design's auto-unblock path).

The manifest's `depends_on_external_ids` becomes `work_item_dependencies` rows on apply, so the spawned implementation tasks have proper dependency structure from the moment they're created.

### What becomes moot if this lands

- **Hand-typed Follow-up Implementation Chores lists.** Replaced by the manifest.
- **Docs-only PRs as the merge mechanism for design docs.** The auto-rebase design (PR #249) and work-dependencies design (PR #234) are the last ones that needed PRs. Future docs go direct-to-main per Q4.
- **The kanban's "open" handler distinguishing design-kind cards from PR-kind cards** is new work, but it replaces the awkward "this card has a PR but the PR is just a markdown change" state today.

---

## Design Question 12 — Failure Modes

### Doc-commit-to-main fails

The worker's `jj git push --bookmark <docs_branch>` fails. Causes:

- **Network blip / GitHub 5xx.** Retry once, then surface as a `WorkAttentionItem` ("Push to <docs_branch> failed: <reason>; design row remains `active`"). User probes the worker to retry.
- **Push rejected (branch protection).** This means `<docs_branch>` is protected and direct-push isn't allowed in this environment — Q4 said configure the product to a different branch. Surface as a `WorkAttentionItem` with explicit advice: *"Branch `<docs_branch>` is push-protected. Either configure `products.docs_branch` to a writable branch, or unprotect this branch for design pushes."* Do not silently fall back to a PR — the policy is design = direct push.
- **Conflict (someone else just pushed to `main`).** `jj git fetch && jj rebase -d main` and retry once. The worker's spawn prompt should include this retry loop. After two failed retries, attention item.

The design row stays in `active` until the worker successfully pushes and prints `DOC_REF:`.

### Approval triggers project/task creation that races with manual creation

The user just clicked Approve. Concurrently, the user (in another window) creates a project named `design-producing-tasks-impl` manually. Engine apply step (Q8 step 3) sees the slug already exists, skips, records in `skipped_existing`. The user gets an `ApproveDesignOutput` showing 1 skipped, sees the project they just made, no harm.

### Worker writes the doc but is interrupted before stopping cleanly

Two sub-cases:

- **Doc written + pushed, no `DOC_REF:` printed.** `DesignDetector` finds nothing; row stays `active`. The user re-runs the design task (resume or new spawn); the worker sees the doc already exists, doesn't re-write it, prints `DOC_REF:` and stops. Idempotent.
- **Doc written but not pushed.** Local-only state; the next worker session starts from a clean fetch and writes the doc again. No harm.
- **Doc + manifest written + pushed, but worker died after push and before printing the sentinel.** Same as the first sub-case — re-spawn, the next worker sees the doc, prints the sentinel.

### Approve called on a row whose doc has been edited under us

The engine recorded `manifest_sha = abc123` at detection. The user clicks Approve. Engine fetches the doc / manifest, computes sha, gets `def456`. Mismatch → engine refuses to apply, surfaces a `WorkAttentionItem`: *"The design's manifest has been edited since detection. Re-detect by setting status back to `active` (the worker will produce a fresh `DOC_REF:`) or accept the new manifest by clicking Approve again with `manifest_sha = def456`."*

The CLI / app's Approve action passes the *currently-displayed* `manifest_sha` from the renderer footer; on mismatch the engine re-checks with the new sha, then proceeds. This guards against approving a stale manifest while gracefully handling legitimate edits.

### Apply step partially fails mid-transaction

Single SQL transaction, all-or-nothing. On rollback, the design row is back to `in_review`, no projects / tasks were created, attention item explains the SQL error. User retries. The transaction's idempotency (Q8) means retry is safe.

### User approves, then realises it was a mistake

`ApproveDesignInput::revoke = true` reverses:

- Design row's status → back to `in_review`.
- `metadata.design.approved_at` cleared.
- The spawned projects / tasks **stay** — revocation does not delete user data. The user can manually delete them via the existing `boss task delete` etc. The design row is now re-approvable; re-approval respects slug dedup and re-creates only what's missing.

This is the conservative version. A more aggressive "revoke + delete spawned rows" path is risky (the user may have already started work on a spawned chore) and out of scope for v1.

### Manifest schema_version mismatch

The manifest's `schema_version` is older than what the engine knows. v1 supports `schema_version = 1`; future versions add a migration step or a "minimum supported version" gate. If the version is unknown / unsupported, the apply fails with a clear error; the doc is still readable in the renderer.

### Renderer cannot fetch the doc

Network failure while loading; or the `gh` token doesn't have read access; or the file was renamed. Renderer shows an error state with a "Retry" button and surfaces the underlying error message. Does *not* mark the design row as failed — the row's status is determined by the engine, not the renderer.

### Doc file is moved or deleted between detection and approval

The renderer 404s. The engine's Approve handler also 404s on its manifest fetch (`gh api /contents/...`). Approve fails with `WorkAttentionItem`: *"Doc at <doc_ref> no longer exists. Either restore the doc or move the design row back to `active` and let the worker re-create it."*

---

## Sequence Diagram — Happy Path

```
┌────────┐  ┌──────┐  ┌──────────┐  ┌────────────┐  ┌────────┐  ┌──────┐  ┌─────────┐
│ user   │  │ kanb │  │ engine   │  │ worker     │  │ jj/gh  │  │ rndr │  │ sqlite  │
│ (CLI/  │  │ an   │  │          │  │ (Claude)   │  │        │  │ win  │  │         │
│  app)  │  │      │  │          │  │            │  │        │  │      │  │         │
└───┬────┘  └──┬───┘  └────┬─────┘  └─────┬──────┘  └───┬────┘  └──┬───┘  └────┬────┘
    │ create design        │              │             │          │           │
    │  (kind='design',     │              │             │          │           │
    │   slug='foo')        │              │             │          │           │
    │ ───────────────────► │              │             │          │           │
    │                      │ INSERT tasks │             │          │           │
    │                      │ ────────────────────────────────────────────────► │
    │                      │ status='todo'│             │          │           │
    │                      │ auto-dispatcher: ready     │          │           │
    │                      │ spawn worker │             │          │           │
    │                      │ ─────────────────────────► │          │           │
    │                      │              │ writes doc.md           │           │
    │                      │              │ writes doc.tasks.json   │           │
    │                      │              │ jj describe + push     │          │
    │                      │              │ ───────────► (push to main)        │
    │                      │              │ STOP w/ DOC_REF:        │          │
    │                      │              │ <─────────── │          │           │
    │                      │ DesignDetector: parses sentinel, validates,        │
    │                      │ stashes manifest_sha                               │
    │                      │ UPDATE tasks SET status='in_review', doc_ref=...   │
    │                      │ ────────────────────────────────────────────────► │
    │                      │ work_item_changed event   │          │           │
    │                      │ ◄─────────── │            │          │           │
    │ kanban shows card in 'In Review' lane (with ↗ doc link)     │           │
    │ ◄───────────────────── │            │            │          │           │
    │ user clicks card     │              │            │          │           │
    │ ─────────────────────►│              │            │          │           │
    │                      │ open renderer window                  │           │
    │                      │ ──────────────────────────────────────►          │
    │                      │              │            │          │ GetDesign │
    │                      │              │            │          │ Doc       │
    │                      │              │            │          │ ───────► │
    │                      │              │            │          │ doc str  │
    │                      │              │            │          │ ◄─────── │
    │                      │              │            │          │ render   │
    │ user reads doc, clicks Approve      │            │          │          │
    │ ────────────────────────────────────────────────────────────►          │
    │                      │              │ ApproveDesign(task_id, sha)        │
    │                      │ ◄────────────────────────────────────│          │
    │                      │ validate manifest_sha unchanged                  │
    │                      │ open transaction                                 │
    │                      │ INSERT projects + tasks + dep edges              │
    │                      │ ────────────────────────────────────────────────►│
    │                      │ UPDATE design row → status='done'                │
    │                      │ ────────────────────────────────────────────────►│
    │                      │ commit transaction                               │
    │                      │ work_items_created + work_item_changed events    │
    │ ◄──────────────────────────────────────────────────────────────────────│
    │ kanban: design done, new projects/tasks visible    │          │           │
```

---

## Schema and Wire Summary

### Column adds

```sql
ALTER TABLE tasks    ADD COLUMN doc_ref TEXT;       -- artifact reference for kind='design'
ALTER TABLE products ADD COLUMN docs_location TEXT; -- 'tools/boss/docs/designs/' or
                                                    -- 'https://github.com/foo/docs.git#designs/'
ALTER TABLE products ADD COLUMN docs_branch   TEXT NOT NULL DEFAULT 'main';
```

`tasks.metadata` JSON gains for `kind = 'design'` rows: `design.{doc_ref, doc_branch, doc_repo_remote_url, manifest_path, manifest_sha, approved_at, approved_by, last_approved_sha}`.

`tasks.kind` gains the value `'design'`. No CHECK constraint is added (the column is already free-form TEXT) — validation happens in the application layer where it always has.

Bump `metadata.schema_version`.

### Protocol additions (`tools/boss/protocol/src/types.rs` + `wire.rs`)

```rust
// types.rs
pub struct ApproveDesignInput {
    pub task_id: String,
    pub manifest_sha: Option<String>,
    pub revoke: bool,
}

pub struct ApproveDesignOutput {
    pub task: Task,
    pub created_projects: Vec<Project>,
    pub created_tasks: Vec<Task>,
    pub skipped_existing: Vec<String>,
}

pub struct GetDesignDocInput {
    pub task_id: String,
}

pub struct GetDesignDocOutput {
    pub doc_ref: String,
    pub branch: String,
    pub repo_remote_url: String,
    pub content: String,
    pub manifest_summary: ManifestSummary,
    pub manifest_sha: String,
}

pub struct ManifestSummary {
    pub project_count: usize,
    pub task_count: usize,
    pub projects: Vec<ManifestProjectSummary>,
    pub tasks: Vec<ManifestTaskSummary>,
}

pub struct ManifestProjectSummary { pub external_id: String, pub name: String, pub slug: String }
pub struct ManifestTaskSummary {
    pub external_id: String,
    pub kind: String,
    pub name: String,
    pub project_external_id: Option<String>,
    pub depends_on_external_ids: Vec<String>,
}

// Product gains:
pub struct Product {
    /* ... existing fields ... */
    pub docs_location: Option<String>,
    pub docs_branch:   String,
}

// wire.rs
ApproveDesign { request_id: String, input: ApproveDesignInput }
GetDesignDoc  { request_id: String, input: GetDesignDocInput }
```

CLI noun:

```
boss design create  <product> --slug <s> --name <n> [--description <d>]
boss design list    [--product <selector>] [--status <s>]
boss design show    <selector>
boss design approve <selector>          # implies revoke=false
boss design revoke  <selector>          # ApproveDesign with revoke=true
boss design convert <task_id> --doc <path>   # bootstrap conversion (Q10)
boss product update <selector> --docs-location <path|url> --docs-branch <b>
```

### Topic / event

Reuse `work_item_changed` for the design row's status flips. Add a single new event topic `engine.design_manifest_applied` that publishes the apply result (created project/task ids) — the kanban subscribes to refresh its lane data without doing a full re-list.

### Engine module split

- `engine/src/design.rs` — new module. `DesignDetector` trait + `CommandDesignDetector` impl. `apply_design_manifest(task_id) -> ApplyResult`. Manifest schema types live here.
- `engine/src/completion.rs` — extend the on-Stop branch to dispatch to `DesignDetector` for `kind = 'design'` rows.
- `engine/src/spawn_flow.rs` — extend the spawn-prompt template with the design-kind addendum (Q4).
- `engine/src/work.rs` — add `WorkDb::insert_design_doc_ref`, `update_design_manifest_sha`, `apply_design_manifest_tx`, plus `set_product_docs_location` / `set_product_docs_branch`.
- `engine/src/rpc.rs` (or wherever the RPC dispatch lives) — handle `ApproveDesign`, `GetDesignDoc`, `RevokeDesignApproval` (or fold revoke into Approve).

### App / UI deltas

- `Models.swift` — mirror `ApproveDesignInput`/`Output`, `GetDesignDocOutput`, `ManifestSummary`, the new `Product.docs_location`/`docs_branch`, and the `'design'` kind value.
- `ContentView.swift` — kanban card variant for `kind = 'design'`: card icon swap (book / doc, vs the current pull-request icon); "open" affordance opens the renderer window instead of a URL.
- New `DesignRendererView.swift` (or similar) — Textual-rendered window with header / footer / Approve button.
- Settings: per-product `docs_location` / `docs_branch` editor.

---

## Risks

**R1 — Worker forgets the manifest.** A worker writes the doc but skips the `tasks.json` sibling. v1's fallback (Q8 δ) is "spawn an extractor worker on approval" but that's deferred to a follow-up. Until then, an `in_review` design with no manifest can't be approved. Mitigation: the spawn prompt is explicit and includes a worked example; `DesignDetector` warns (via attention item) when the manifest is missing so the user catches it before clicking Approve.

**R2 — Manifest schema drift between worker and engine.** The worker emits `schema_version = 1` JSON; later we want to evolve. Mitigation: the engine refuses unknown `schema_version` values with a clear error (Q12) and the schema is small / versioned from day one. New fields are added as `Option` to keep older manifests valid; breaking renames bump the version.

**R3 — `Textual` rendering gaps.** GFM features Textual doesn't support (PR-number autolinks, GitHub task lists, mermaid diagrams) render as plain text. Mitigation: the existing `tools/boss/docs/designs/*.md` corpus uses only CommonMark + tables + fenced code, all of which Textual handles. Document the gap; upgrade to `WKWebView` if/when an unsupported feature becomes load-bearing for the doc style.

**R4 — Direct-to-main pushes from many concurrent design workers.** Multiple workers writing different docs concurrently, all pushing to `main`, each force-pushing causes lost-update-style races. Mitigation: workers do `jj git fetch && jj rebase -d main` before push; conflicts cause the worker to retry; the spawn prompt's retry loop handles transient cases. This is the same pattern the auto-rebase doc Q4 R2 covers.

**R5 — Approval applied from a stale renderer.** The user has the renderer open from yesterday; they click Approve; the manifest in the repo has been edited since. Q12's `manifest_sha` check catches this — the engine refuses, the renderer surfaces the conflict, the user reloads.

**R6 — Bootstrap path is awkward.** This very chore is a `chore`, not yet a `design`, and its follow-up implementation chores have to be hand-enqueued. Mitigation: Q10 documents the path; the awkwardness is one-time; future design tasks benefit.

**R7 — Doc location split between repo + branch confuses users.** A design's effective doc location is `<docs_repo>@<branch>:<docs_location>/<slug>.md` — three configurable axes. Mitigation: `boss design show` renders the resolved path explicitly; the kanban card's tooltip shows the same; defaults (same repo, `main`, `tools/boss/docs/designs/`) cover ~all cases.

**R8 — Re-approval drops tasks the design abandoned.** Q9's "additive only" rule means tasks the v2 manifest dropped from the v1 manifest stay in the work-item DB indefinitely. Mitigation: this is by design (preserve the user's history); a future cleanup verb (`boss design diff-pending <task_id>` showing what'd be removed if we made it destructive) is a follow-up if this turns out to bite.

**R9 — Out-of-band doc edits.** Q5's note about a doc being edited without the design row's worker noticing means the engine's `manifest_sha` will be stale. Q12's mismatch-on-Approve handler is the safety net but the user has to re-detect. Mitigation: a future "watch the doc file for changes and re-detect automatically" worker is filed as out-of-scope.

**R10 — Concurrent Approves.** Two windows both click Approve at once. The transaction in Q8 is the answer: SQL serialises; the second commit either no-ops (idempotent) or fails on `manifest_sha` already cleared. v1 trusts SQLite-level isolation.

---

## Follow-up Implementation Chores (to enqueue once approved)

Bite-sized; each fits one worker session. Once the manifest-driven flow lands, future design tasks generate this list automatically — but until then, the list below is the canonical hand-typed manifest for *this* design.

1. **Schema + migration**: add `tasks.doc_ref` column, `products.docs_location` + `docs_branch` columns, bump `metadata.schema_version`. Idempotent migration; existing rows default `docs_branch = 'main'`. Acceptance: fresh init and migration both yield the new schema; existing rows are unchanged; new column has correct defaults.

2. **Protocol types**: `ApproveDesignInput`/`Output`, `GetDesignDocInput`/`Output`, `ManifestSummary`, `Product.docs_location`/`docs_branch`, plus the `'design'` kind value docs. Mirror in `Models.swift`. Acceptance: serde / Codable round-trips green; existing wire tests still pass.

3. **Engine: `DesignDetector` trait + `CommandDesignDetector`** — sentinel parsing, doc/manifest existence checks via `gh api`, sha capture. Stub for tests. Acceptance: unit tests cover sentinel match / no-match, doc-404, manifest-404, multi-line transcript ending.

4. **Engine: spawn-prompt addendum** for `kind = 'design'` — render the direct-push instructions and the manifest schema example into the spawn prompt template. Acceptance: spawning a `design` row produces a prompt containing the addendum; prompt-snapshot tests cover the substitutions.

5. **Engine: on-Stop dispatch** — `completion.rs` branches on `task.kind == 'design'` to call `DesignDetector` instead of `PrDetector`. Acceptance: integration test simulates a design worker stopping with a `DOC_REF:` line; row flips to `in_review`; `doc_ref` column is set; `manifest_sha` is captured.

6. **Engine: manifest schema + parser** — Rust types matching the v1 manifest, validation (topo-sort, cycle detection, required-field checks). Acceptance: golden-file tests for valid + invalid manifests; cycle detection rejects with a clear error.

7. **Engine: `apply_design_manifest_tx`** — single-transaction insert path with `(slug, product_id)` dedup, dependency-edge wiring, `last_approved_sha` snapshotting. Acceptance: integration test creates a design row, inserts a manifest, calls apply, asserts projects + tasks + edges materialise; second call (idempotency) is a no-op; manifest-sha mismatch refuses to apply.

8. **Engine: `ApproveDesign` RPC** — wires the trigger into the wire layer, calls the apply path, broadcasts events. Acceptance: CLI / app integration test: `boss design approve <selector>` after `in_review` flips the row to `done` and prints the created rows; `--revoke` flips back without deleting.

9. **Engine: `GetDesignDoc` RPC** — returns the rendered doc's content + manifest summary. Implementation: prefer leased-workspace filesystem read, fall back to `gh api /contents`. Acceptance: returns content + summary for both same-repo and separate-repo `docs_location` configs.

10. **CLI: `boss design create | list | show | approve | revoke | convert`** — full noun. Acceptance: `--help` covers every verb; integration test covers create → spawn → in_review → approve flow.

11. **CLI: `boss product update --docs-location <p> --docs-branch <b>`** — settings setters. Acceptance: setter persists; subsequent design creates inherit.

12. **macOS: kanban card variant** for `kind = 'design'` — icon, hover-menu Approve, "open" opens renderer window. Acceptance: snapshot tests for the new card; existing PR cards unchanged.

13. **macOS: `DesignRendererView`** — Textual-based renderer window with header (title, status, Approve) and footer (manifest summary). Acceptance: snapshot tests against fixture docs (including this design doc); Approve button enabled / disabled state matches Q7 rules.

14. **macOS: per-product `docs_location` / `docs_branch` editor** in product settings. Acceptance: edits persist; `boss product show` reflects them.

15. **Worker manifest fallback (β)** — on Approve, if `metadata.design.manifest_path` is missing, spawn a manifest-extractor worker that emits the manifest from the doc. Acceptance: integration test against a manifest-less doc spawns the extractor; output is applied like a worker-emitted manifest.

16. **Bootstrap conversion**: `boss design convert <task_id> --doc <path>` to retroactively flip this very row's `kind` from `chore` to `design`. Acceptance: converts in place, populates `metadata.design.*` from the existing doc, no data loss.

17. **(Optional follow-up) Renderer diff toggle** — Q9 (♥). "Show changes since last approval" view in the renderer.

18. **(Optional follow-up) Inline commenting** — picked up by the doc-collab project (`proj_18a2bb78815be670_3`).

19. **(Optional follow-up) Cross-repo doc location resolution** — multi-repo workspaces and authority delegation across orgs. Out of v1 hard scope.

---

## Out of Scope

- Inline / threaded comments on rendered docs (handled by the doc-collab project).
- Edit-existing-doc diff view (deferred to follow-up; v1 renders the file as-is).
- Cross-product / cross-repo doc location resolution beyond the same-repo + separate-repo cases.
- Replacing PRs for code-modifying work — design tasks are docs-only by definition.
- Automatic re-detection on out-of-band doc edits.
- Destructive re-approval (deleting tasks the new manifest dropped).
- Multi-account `gh` authentication for separate-repo doc pushes.
- Approval workflow with multiple approvers / required-reviewers.
- Rendering of GitHub-Flavoured-Markdown features Textual doesn't support (autolinks, task-lists, mermaid).
- Persistent renderer-window state across restarts.
