# Boss: Revision Tasks

## Problem

Every work-item kind Boss has today produces a *new* artifact. A `chore` / `project_task` produces a new PR; an `investigation` / `design` produces a new markdown doc. The lifecycle plumbing reflects this: a worker pushes to its own engine-supplied bookmark (`boss/exec_<id>_<seq>`), runs `gh pr create`, and the completion detector flips the row to `in_review` when a *new* PR URL appears (`runner.rs` spawn prelude, `runner.rs:750-758`; `PrDetector` in `completion.rs`).

There is no kind whose deliverable is *another commit on an existing PR*. Yet that is exactly the shape of the most common follow-up the operator asks for: "revise T651 to also handle the empty-list case", "for T652, can we rename that flag before it merges". Today the operator's only options are (a) reopen the merged-or-unmerged work as a brand-new chore, which produces a *second* PR that has to be merged separately and loses the reviewer's in-context thread, or (b) drop into the worker's workspace by hand and push a commit themselves. Both fight the grain of the system.

This is also the substrate a future feature needs. A separate effort will let the operator triage GitHub PR-review comments on an in-review parent PR and act on selected ones. When that lands, each "act on this comment" decision should *create a revision task* — the same mechanism this design defines — rather than inventing a parallel path. So the mechanism here has to be clean enough that the comment-triage UI is a thin producer on top of it.

This doc proposes a first-class **`revision`** task kind: bound to a parent task, gated on the parent's PR being open and unmerged, dispatched into (ideally) the parent's warm workspace, and delivering a *new commit on the parent's existing branch* — no new PR. Revision tasks render as distinct cards while in Backlog/Doing and roll up under the parent's card as single-line affordances once they reach Review.

## Goals

- A new `tasks.kind = 'revision'` whose deliverable is **a new commit on the parent task's existing PR branch**, not a new PR.
- A **parent linkage** column on `tasks` that ties a revision to the task whose PR it revises, with the DB-adjacent invariant "kind = revision ⇒ parent is set".
- **Two trigger sources, one mechanism.** Source A (direct operator feedback on an in-review PR) is built now. Source B (GitHub PR-review-comment triage UI) is deferred, but the create path must be the substrate B drives — B is a thin producer, not a fork.
- A **gate** that refuses to create or dispatch a revision unless the parent has an *open, unmerged* PR. Enforced at create time *and* re-checked at dispatch time (the PR can merge in between).
- **Sequence numbers** (R1, R2, R3…) that are stable across reordering and meaningful to a human reading the kanban.
- A **dispatch flow** that checks out the parent's branch by name (not a fresh `boss/exec_*` bookmark), pushes back to it, and never calls `gh pr create`.
- **Cube workspace warmth**: prefer the workspace the parent last ran in, degrade gracefully when it is gone or leased.
- **Kanban chrome**: distinct revision cards in Backlog/Doing (R-badge + short revision description); a per-revision single-line affordance on the parent's card in Review. No new column.
- A **coordinator system-prompt paragraph** that teaches the Boss the verb exists and when to reach for it, without hard-coding keyphrases.
- An **effort default** for the kind that matches its usual narrowness, with documented escalation.

## Non-Goals

- **Building the Source-B comment-triage UI.** The mechanism must be extensible to it (the design anticipates the `(repo, pr#, comment-id)` pointer shape per [[feedback_github_is_source_of_truth_for_pr_artifacts]]) but the surface itself is a separate effort.
- **Auto-applying review comments** without a human gate. Operator-confirmed only.
- **Cross-PR revision.** One revision = one commit to the *one* PR owned by its parent chain. A revision that wants to touch a different PR is a different parent.
- **Auto-rebasing a revision onto a moved-on main.** If main advanced between parent push and revision push, the worker rebases the normal way (the existing conflict-resolution flow) or fails loud. No new rebase machinery.
- **Rewriting the parent task's brief.** Revisions add commits; they do not retroactively edit the parent task's description.
- **A "revisions" kanban column.** Revisions flow through the existing Backlog/Doing/Review columns with different card chrome.
- **Auto-merging the parent PR** after N successful revisions. Always human-merged.
- **A dedicated `tasks.pr_url` of the revision's own.** A revision has no PR of its own; its artifact is a commit on the parent's PR. The revision row's `pr_url` stays `NULL`; the parent's `pr_url` is the source of truth.

## Naming

- **Kind**: `revision` (`tasks.kind = 'revision'`). CLI noun lives under the existing `boss task` umbrella: `boss task create-revision …`, mirroring `boss task create-investigation`.
- **Execution kind**: `revision_implementation` (the value carried on `executions.kind`, mirroring `investigation_implementation`). This is what the runner matches on to select the spawn directive.
- **Parent linkage**: `tasks.parent_task_id` — a soft foreign key to the `tasks` row whose PR this revision targets. `NULL` for every non-revision row.
- **Chain root**: the first non-revision ancestor reached by walking `parent_task_id` up. This is the task that *owns* the PR. Revisions, revisions-of-revisions, etc. all share one chain root and one PR.
- **Sequence number**: `R<n>`, surfaced in UI text as "R1" / "Revision 1" — never "Friendly ID" anywhere, per [[feedback_no_friendly_id_in_ui]]. Computed, not stored (Q1).
- **Revision intent**: the operator's verbatim ask (Source A) or a short generated summary (Source B), stored in the existing `tasks.description`. This is what the Review-lane affordance renders, so it must stay short.

---

## Design Question 1 — Kind, Parent Linkage, and Sequence Numbers

### Options

- **(a) New `kind = 'revision'` + nullable `parent_task_id` column; sequence computed at render.** Same `tasks` table, one new column, one new discriminator value. Walks the parent chain to find the PR.
- **(b) Boolean `is_revision` flag layered on an existing kind (e.g. `chore`).** Keep the discriminator; add a bit.
- **(c) New table `revisions` foreign-keyed to `tasks`.** A parallel row type with its own lifecycle column.
- **(d) Encode parent + sequence in `tasks.description` / a JSON blob.** No schema change.

Sub-question on the sequence number itself:
- **(s1) Stored `revision_seq` column**, assigned at create.
- **(s2) Computed** as `(count of revision-kind siblings under the same chain root created before me)`, derived at read time.

### Discussion

The repo has already litigated (a) vs (b) vs (c) vs (d) twice — see `design-producing-tasks.md` Q1 and the investigation kind (T641). The verdict each time was (a): `kind` is the established "what shape of work is this" discriminator (`chore`, `investigation`, `project_task`, `design` today; `work.rs:1622-1656`), it is free-form `TEXT` with no enum and no CHECK constraint, and validation lives in the application layer. Adding `revision` is the natural fourth-ish extension. (b) doubles the discriminator and forces every `match task.kind` site to also test a bit; (c) duplicates executions / transcripts / attention-item / dispatch plumbing that all key on `tasks.id`; (d) hides the row's type inside a string.

The genuinely new thing a revision needs that prior kinds did not is the **parent edge**. There is no self-referencing FK on `tasks` today (`project_id` points at `projects`, `blocked_attempt_id` at a conflict attempt — neither is task→task). So this design adds one: `parent_task_id TEXT NULL`, soft FK to `tasks.id`, mirroring how `project_id` is a soft reference (no `REFERENCES` clause — Boss uses soft deletes and application-layer integrity, consistent with the rest of the schema).

The invariant "kind = revision ⇒ parent_task_id IS NOT NULL" is real and worth enforcing. SQLite supports a partial-ish CHECK: `CHECK (kind <> 'revision' OR parent_task_id IS NOT NULL)`. But Boss's existing tables carry **no** CHECK constraints (the schema deliberately keeps validation in Rust so error messages are good and migrations stay `ALTER TABLE … ADD COLUMN`-shaped; see the `migrate_tasks_*` family, e.g. `migrate_tasks_investigation_doc_columns` at `work.rs:8156`). Adding a CHECK to a *new column on an existing table* via `ALTER TABLE ADD COLUMN … CHECK(…)` is legal in SQLite but the constraint only re-validates on insert/update, and a table-level cross-column CHECK can't be added by `ALTER TABLE` at all without a table rebuild. So the recommendation keeps the invariant in the application layer (the `insert_revision_in_tx` constructor refuses a null parent; an `update` that would orphan a revision is rejected) and documents it as a column comment — consistent with how every other `tasks` invariant is enforced.

On the sequence number: (s1) stored is tempting but goes stale the moment anything is reordered, deleted, or soft-deleted, and it needs a uniqueness story under concurrency. (s2) computed is stable by construction and matches what the operator means by "R2": *the second revision in this chain, in creation order*. The count is cheap (`parent_task_id` chain is short) and the kanban already recomputes derived state on each `WorkTree` push. The number that matters for display is "position in the chain root's revision list, ordered by `created_at`", which is deterministic and reorder-proof because `created_at` never changes.

One subtlety: "count under the same *chain root*" vs "count under the same *immediate parent*". Revision chains (Q-open) mean R2 can itself get a revision. The chain-root count gives R1, R2, R3 across the whole chain (a revision-of-R2 is R3, not "R2.R1"); the immediate-parent count would reset. The chain-root reading is what a human scanning the parent card wants ("this PR has had 3 rounds of revision"), so the sequence is **chain-root-scoped, creation-ordered**. This is justified again under the revision-chains open question.

### Recommendation

**Pick (a) + (s2).**

```sql
ALTER TABLE tasks ADD COLUMN parent_task_id TEXT;  -- soft FK → tasks.id;
                                                   -- NULL for non-revision rows;
                                                   -- required (app-enforced) when kind='revision'
CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id ON tasks(parent_task_id);
```

Migration mirrors `migrate_tasks_investigation_doc_columns` exactly — a `migrate_tasks_parent_task_id_column(conn)` that guards with `table_has_column(conn, "tasks", "parent_task_id")?` and runs the single `ALTER TABLE … ADD COLUMN`. Add the index in the same migration. No CHECK constraint; the invariant is enforced in `insert_revision_in_tx` and on update.

`kind` gains the value `'revision'`. No enum to extend (it is `TEXT`); the new value is recognized in the dispatch reconcile loop (Q3) and the completion path (Q4).

Sequence number is computed:

```sql
-- R<n> for revision task :id, n = its 1-based position among the chain
-- root's revisions ordered by created_at.
SELECT COUNT(*) + 1
  FROM tasks
 WHERE kind = 'revision'
   AND chain_root_id(parent_task_id) = chain_root_id(:parent_task_id_of_id)
   AND created_at < (SELECT created_at FROM tasks WHERE id = :id);
```

`chain_root_id` is a small recursive walk (`WITH RECURSIVE`, or a Rust helper `WorkDb::chain_root(task_id)` — preferred, because the walk is short and a Rust helper is easier to test and reuse than a recursive CTE embedded in every query). The kanban receives the computed `R<n>` as a derived field on the task projection so the UI never recomputes it.

#### Kind-of-revision: column or description?

The brief asks whether review-comment-driven vs operator-driven should be its own enum. **Recommendation: do not add a sub-kind column for v1.** The distinction is provenance, and Boss already records provenance in `tasks.created_via` (`canonicalize_created_via(..., "<kind>")`, `work.rs:7052,7092`). Source A sets `created_via = "operator"` (or the human's actor); Source B will set `created_via = "pr-comment:<repo>#<pr>:<comment-id>"`, which doubles as the `(repo, pr#, comment-id)` pointer [[feedback_github_is_source_of_truth_for_pr_artifacts]] wants without mirroring the comment body into Boss state. No schema change needed when B lands; B just passes a richer `created_via`. If a hard enum is ever wanted, it is an additive `revision_source TEXT` column later.

---

## Design Question 2 — CLI Surface

### Options

- **(a) `boss task create-revision --parent <task-selector> --description "…"`** — a sibling verb to `create-investigation`, with its own arg struct.
- **(b) `boss task create --kind revision --parent <selector> …`** — overload the generic `task create`.
- **(c) `boss revision create …`** — a new top-level noun.

### Discussion

`boss task create-investigation` already exists as a dedicated verb with its own `InvestigationCreateArgs` (`cli/src/main.rs`; create dispatch around `main.rs:2543`). There is no generic `boss task create --kind X` today — each kind got its own verb. Following that established shape ((a)) keeps the parser honest: the revision verb *requires* `--parent`, which a generic `--kind revision` flag could not enforce structurally. (b) would have to validate "if kind=revision then parent is required" at runtime, re-introducing the exact ambiguity the per-verb pattern avoids. (c) invents a top-level noun for something that is fundamentally a task; the existing nouns are `task`, `project`, `product`, and revisions live under `task`.

### Recommendation

**Pick (a).** A `RevisionCreateArgs` struct modeled on `InvestigationCreateArgs`:

```
boss task create-revision \
    --parent <task-selector>      # required; the task whose PR to revise.
                                  # Accepts the same selector forms as other
                                  # task verbs (short_id like "T651", full id,
                                  # or product/slug).
    --description "<ask>"         # required-ish; the verbatim operator ask or
                                  # a short summary. Stored in tasks.description
                                  # and rendered on the Review-lane affordance,
                                  # so keep it short.
    [--priority <p>]              # defaults to parent's priority
    [--effort <level>]            # defaults to 'small' (Q7); escalatable
    [--model <slug>]              # optional model override
    [--force-duplicate]           # same dedup-bypass flag as other creates
```

`--product`/`--project` are **not** flags: a revision inherits both from its parent (resolved at create time), so passing them would only create disagreement. `--repo` is likewise inherited — a revision must push to the parent's PR branch in the parent's repo; allowing a different repo would violate the one-PR invariant.

#### Error behavior on a bad parent

`create-revision` validates the gate at create time (Q4) and returns a precise `CliError` rather than creating a doomed row:

- `--parent` resolves to a task with **no PR** → `error: T651 has no PR yet; a revision targets an existing open PR. Wait for T651 to reach review, or file a normal follow-up chore.`
- parent's PR is **merged** → `error: T651's PR (#1234) is already merged; revisions only apply to open, unmerged PRs. File a new chore against main instead.`
- parent's PR is **closed-unmerged** → `error: T651's PR (#1234) is closed without merging; there is no open PR to revise.`
- `--parent` resolves to a `revision` itself → allowed (revision chains, Q-open); the gate is evaluated against the *chain root's* PR.

These messages come from a single `assert_parent_revisable(parent_chain_root) -> Result<(), RevisionGateError>` helper shared by the create path and the dispatch path (Q4), so the wording is identical wherever the gate fires.

---

## Design Question 3 — Dispatch Flow

This is the heart of the design: a revision worker must behave *unlike* every worker before it.

### What a normal worker is told today

The spawn prelude (`runner.rs` `spawn_prompt`, matching on `execution.kind` at `runner.rs:700`) hands a fresh-PR worker the block at `runner.rs:751-758`: create bookmark `boss/exec_<id>_<seq>`, `jj git push -b <that> --allow-new`, `gh pr create --head <that> --base main`. There is *already* a divergent branch in that same function (`runner.rs:741-749`) for the case where the chore **already has a `pr_url`**: it tells the worker to push to the existing PR branch and NOT open a new PR, confirming with `gh pr view <n>`. The revision flow is a specialization of that existing "resume existing PR" branch, but the PR belongs to the *parent*, not to this execution.

### Options for selecting the divergent prelude

- **(a) Add a `revision_implementation` arm to the `match execution.kind`** in `spawn_prompt`, with a `compose_revision_directive(parent)` that names the parent's branch and forbids `gh pr create`. Mirrors `compose_investigation_directive` (`runner.rs:809`).
- **(b) Reuse the existing `existing_pr_url` branch** (`runner.rs:741`) by populating the revision execution's "existing PR" slot from the parent's `pr_url`.
- **(c) A post-hoc engine fast-forward** — worker pushes a normal feature branch, engine grafts it onto the parent's branch.

### Discussion

(c) is rejected for the same reasons `design-producing-tasks.md` Q4 rejected engine-side `jj`: running git plumbing in engine code outside a leased workspace is fragile and we have a standing decision against it. (b) is appealing — the "existing PR" prelude already says the right words ("push to the existing branch, don't open a new PR") — but the parent's `pr_url`/branch is not *this* execution's, and the acceptance-criterion block keys off `existing_pr_url` being the execution's own. Threading the parent's branch through that slot blurs "this task's PR" with "the parent's PR", which matters for the completion detector (Q4). (a) is explicit and mirrors the established `compose_*_directive` pattern; the runner already branches on `execution.kind`, so one more arm is the low-surprise change.

### Recommendation

**Pick (a).** Add to the `match execution.kind` in `spawn_prompt`:

```rust
"revision_implementation" => {
    prompt.push_str(&compose_revision_directive(parent_task, parent_pr));
}
```

and add `revision_implementation` to the acceptance-criterion `matches!` set (`runner.rs:718-721`) so the worker still gets the "deliverable is a pushed branch" framing — but `compose_revision_directive` supplies the branch name and *suppresses* the new-PR instructions.

#### `compose_revision_directive` — the worker's marching orders

```
Expected outcome for this run:
- This is a REVISION task. Your deliverable is a NEW COMMIT on an EXISTING
  pull request — the PR produced by the parent task <PARENT_SHORT_ID>. Do NOT
  open a new PR. Do NOT create a `boss/exec_*` bookmark.
- The parent PR is #<PR_NUMBER> on branch `<PARENT_BRANCH>` in <REPO>.
- What this revision should change: <DESCRIPTION>.

Steps:
1. `jj git fetch`   # the parent branch lives on GitHub; this workspace may
                    # not have it locally even if it was warmed for the parent.
2. `jj edit <PARENT_BRANCH>`   # move onto the existing branch head.
3. `jj new @`                  # start a fresh commit on top of it.
4. Make the requested change.
5. `jj describe -m "<short message>: <DESCRIPTION>"`.
6. `jj bookmark set <PARENT_BRANCH> -r @`   # advance the bookmark to your new commit.
7. `GIT_DIR=.jj/repo/store/git jj git push -b <PARENT_BRANCH>`  # NO --allow-new;
                                                                # the branch exists.
8. Confirm the new commit is on the PR:
   `GIT_DIR=.jj/repo/store/git gh pr view <PR_NUMBER>`.
9. (Recommended — Q-open) post a tracking comment on the PR:
   `gh pr comment <PR_NUMBER> --body "[boss-revision] R<n>: <DESCRIPTION>"`.
10. Print the parent PR URL on its own line as the final thing in your response.
```

The `<PARENT_BRANCH>`, `<PR_NUMBER>`, `<REPO>`, and `R<n>` are resolved engine-side at spawn from the chain root's `pr_url` and the computed sequence number, so the worker never has to discover them.

#### Why `jj git fetch` first is load-bearing

The brief flags this explicitly. The parent's branch was pushed to GitHub by the parent worker. The revision worker may land in:
- **the parent's actual warm workspace** (best case, Q-cube) — the branch is present locally, but may be behind GitHub if anything else pushed; `jj git fetch` reconciles.
- **a fresh/other workspace** — the branch is not present locally at all; `jj git fetch` is what brings it down so `jj edit <PARENT_BRANCH>` resolves.

Either way `jj git fetch` then `jj edit <PARENT_BRANCH>` works. Step 6 (`jj bookmark set`) + step 7 (push without `--allow-new`) is the jj idiom for "advance an existing remote bookmark", verified against the workspace's own PR-update recipe in `.claude/CLAUDE.md` ("To update an existing PR: `jj git push -b my-feature` — no `--allow-new`").

#### Cube workspace allocation — the precedence rule

The coordinator already supports `--prefer`: `lease_workspace_with_fallback` (`coordinator.rs:1817`) reads `execution.preferred_workspace_id` and, if set, leases with `--prefer <id>` (`coordinator.rs:1836`). Today the only producer of a non-null `preferred_workspace_id` is the orphan-resume path (`work.rs:671`, reusing an orphaned predecessor's `cube_workspace_id`). Revision dispatch becomes the second producer.

The precedence rule for a revision's `preferred_workspace_id`:

1. **The workspace the chain root's most recent successful execution ran in** (`executions.cube_workspace_id` for the latest non-failed execution of the chain root). This is the warmest cache — it built the parent's branch.
2. If that execution has no recorded workspace, **any prior revision's workspace** in the same chain (next-warmest — it has the branch too).
3. If none, **no preference** — lease any free workspace.

**Critical divergence from the orphan-resume semantics.** The existing fallback matrix (`coordinator.rs:1801-1804`) says: *preferred set ⇒ terminal failure if the preferred workspace can't be leased* (to preserve state continuity — the orphan's local commits exist *only* there). That is wrong for revisions: a revision's needed state lives on **GitHub**, recoverable in any workspace via `jj git fetch`. So a revision must use a **soft prefer** — `--prefer` for cache warmth, but fall back to `any_free` on failure rather than failing terminally.

Concretely, this needs a per-execution "prefer is soft" signal so `lease_workspace_with_fallback` picks `fallback_policy = "any_free"` even though a preference is set. The cleanest shape is an additive `executions.prefer_is_soft` boolean (defaulted false; set true for `revision_implementation`), consumed where `fallback_policy` is decided (`coordinator.rs:1825`). The orphan-resume path keeps the hard "none" policy; revisions opt into soft. When a revision lands in a non-preferred workspace, the `jj git fetch` in step 1 of the directive makes it correct regardless. This directly answers the cube-stickiness open question and respects [[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]] — warmth is an optimization, never a correctness dependency.

---

## Design Question 4 — Gate Enforcement and Completion

### The gate: "parent PR open and unmerged"

The gate must hold at two moments, because the PR can merge in between:

1. **Create time** (`create-revision`, and later Source B): reject if the parent chain root's PR is absent / merged / closed-unmerged.
2. **Dispatch time** (the moment the coordinator is about to spawn the revision worker): re-check, because minutes-to-hours can pass in Backlog and the parent may have merged.

### Where the PR state already lives

The merge poller is the single surface that knows a PR's lifecycle: `PrLifecycleState { Open(OpenPrStatus), Merged, ClosedUnmerged }` (`merge_poller.rs:203`). It writes derived state back onto the *task row* via `update_task_pr_poll_state` (`work.rs:3871`) — `pr_state_polled_at`, `ci_required_state`, `review_required_state`, `merge_queue_state` — and flips a merged chore to `done` via `mark_chore_pr_merged` (`work.rs:3821`). The brief is explicit: **do not introduce a parallel polling loop.** Reuse this surface.

### Options

- **(a) Create-time: read the chain root's *cached* poll state* (`pr_state_polled_at` + a derived "is open" reading); Dispatch-time: trust the same cached state, refreshed opportunistically by the existing poller cadence.**
- **(b) Create-time and dispatch-time both do a fresh synchronous `gh pr view` against the parent PR.**
- **(c) Create-time uses cached state; dispatch-time forces one targeted poll of the parent PR through the existing poller (a `poll_now(pr_url)` entrypoint), not a new loop.**

### Discussion

(b) re-polls GitHub twice and adds latency to the CLI create path; it also duplicates the `gh pr view` parsing that the poller owns. (a) is cheapest but risks dispatching a revision against a PR that merged seconds ago and hasn't been re-polled. (c) splits correctly: create time is interactive and a slightly-stale read is fine (the operator just saw the PR in review), while dispatch time is the dangerous moment (the worker is about to edit a possibly-merged branch) and deserves a *fresh, targeted* check — but routed through the poller's existing probe (`PrLifecycleProbe`), not a bespoke call.

### Recommendation

**Pick (c).**

- **Create-time gate**: `assert_parent_revisable` reads the chain root's last-known PR lifecycle. If `pr_url` is `NULL` → reject (no PR). If the cached state is `Merged`/`ClosedUnmerged` → reject. If `Open` (or unpolled-but-present) → allow, and let dispatch do the authoritative re-check. The cached reading is derived from the columns the poller already maintains; a present `pr_url` with no terminal state is treated as open.
- **Dispatch-time gate**: just before the coordinator spawns a `revision_implementation` worker, invoke the poller's existing single-PR probe against the chain root's `pr_url`. On `Merged`/`ClosedUnmerged`, *do not spawn*; instead transition the revision per the merged-mid-flight policy (Q-open) and surface a `WorkAttentionItem`. On `Open`, proceed.

The gate logic is engine-owned, reading engine-maintained PR state — consistent with [[feedback_engine_owns_reconciliation_not_ui]]. The UI never evaluates the gate; it only renders what the engine decided.

### Completion: how a revision reaches `in_review` and `done`

A revision worker pushes a commit to the parent's branch and prints the **parent's** PR URL. The completion detector (`PrDetector`-family) must handle this:

- For a `revision_implementation` execution, the detector does **not** look for a *new* PR. It confirms (via `gh pr view <parent#>`) that the parent PR's head advanced (a new commit is present) and that the printed URL matches the chain root's `pr_url`. On success, the revision row flips to `in_review`. It does **not** touch the parent's status.
- The revision row's own `pr_url` stays `NULL`. The chain root remains the PR's owner.
- **`done`**: a revision is `done` when the parent PR merges. The merge poller already flips the chain root to `done` via `mark_chore_pr_merged`; extend that path so that when a chain root merges, its `in_review` revisions are flipped to `done` in the same transaction (they are rolled up under the parent; their deliverable — the commit — merged with it). A revision that is still in Backlog/Doing when the parent merges hits the merged-mid-flight policy (Q-open) instead.

This keeps the revision's lifecycle entirely engine-driven off existing signals: spawn → push commit → `in_review` (detector) → `done` (parent merge poller). No new poller, no new status column.

### Permission hard-guard

The brief flags that every worker can `gh pr create` today, and a misbehaving revision worker could open a duplicate PR despite the directive. **Recommendation: a hard guard, not just prelude trust.** The worker environment already wraps `gh` behind `GIT_DIR=…` and a permission allowlist. For `revision_implementation` executions, install a thin `gh` wrapper (or a hook on the existing permission layer) that **rejects `gh pr create`** with a message pointing at the revision contract. This is cheap insurance: the directive tells the worker not to, and the guard makes "not to" unbreakable. Implementation note for the follow-up: the guard keys on the execution kind the engine already passes into the worker's environment, so it is a per-execution conditional in the allowlist, not a global block.

---

## Design Question 5 — Kanban UI

Per [[feedback_engine_owns_reconciliation_not_ui]], the engine owns the parent↔revision relationship and computes everything the card needs (the `R<n>` sequence, the chain root id, the parent's PR URL); the kanban renders engine state and never derives the relationship itself. The task projection the app already consumes (`WorkTask` in `Models.swift`, with `kind`, and the investigation pointer fields at `Models.swift:502-511`) gains: `parentTaskId: String?`, `revisionSeq: Int?` (the computed R-number), and `revisionParentPrUrl: String?`.

Cards are rendered in `ContentView.swift` (board at `ContentView.swift:689-748`; per-kind affordances at `ContentView.swift:1711-1736` where the `design` and `investigation` doc-link affordances already live). The revision chrome slots into the same affordance area.

### Backlog / Doing — distinct revision card

A revision in `todo`/`active` renders as its **own** card, visually a sibling of the parent's card but unmistakably a revision:

```
┌──────────────────────────────────────────┐
│ ⟳ R2  ·  revises T651                      │   ← header: revision glyph + R-badge
│                                            │     + "revises <parent short id>"
│ Rename --dry-run to --plan before merge    │   ← tasks.description (the ask), 1–2 lines
│                                            │
│ T651 · #1234  ↗                            │   ← chain-root short id + parent PR number,
│                                            │     click opens the parent PR
│  small · medium-model            ● active  │   ← effort/model/status, same row chrome
└──────────────────────────────────────────┘
```

- **R-badge** (`R2`) sits top-left where other cards show their kind glyph. The `⟳` revision glyph + `R<n>` together signal "this is the 2nd revision of T651". Text reads "R2" / "Revision 2", never "Friendly ID" ([[feedback_no_friendly_id_in_ui]]).
- **Revision-description line** is `tasks.description` truncated to ~2 lines.
- **Parent reference line** shows the chain-root short id and the PR number, and is the click target to open the parent PR (`revisionParentPrUrl`).
- **Same color family as the parent, smaller emphasis.** Not indented under the parent in Backlog/Doing (kanban columns are flat status lanes; indentation would fight the layout). Instead it is tagged: same accent color as the parent's product, with the `⟳ R<n>` chip as the distinguishing mark. This keeps it a "distinct card that resembles the original but clearly shows its revision sequence and a short description", per the brief.

### Review — rolled up under the parent

A revision in `in_review` does **not** render its own card. Instead the **chain root's** Review-lane card gains one single line per revision:

```
┌──────────────────────────────────────────┐
│ T651  Wire up --plan flag        #1234 ↗   │   ← parent card, unchanged header
│ ✓ CI · ◷ review                            │   ← existing CI/review chips
│ ──────────────────────────────────────    │
│ ⟳ R1  addressing review comments      ↗    │   ← one line per in-review revision
│ ⟳ R2  rename --dry-run to --plan      ↗    │     R-badge + short intent + link
└──────────────────────────────────────────┘
```

- Each line is `⟳ R<n>` + the revision's short intent (`tasks.description`, hard-truncated — this is why the brief insists the description stays short, and why the future Source-B description must be terse, e.g. "addressing @alice's comment on foo.rs:42").
- The line's link target: **the parent PR** (`revisionParentPrUrl`). v1 links to the PR itself rather than a specific commit, because the revision row does not store the commit SHA (the worker pushes it; the engine does not capture it). If a future iteration wants per-commit links, the completion detector (Q4) can capture the pushed SHA into a new `revision_commit_sha` column and the line links to `…/pull/<n>/commits/<sha>`. Flagged, not built.
- These lines read from engine state (`parentTaskId` + `revisionSeq` + status), so the app groups in-review revisions under their chain root purely by reading fields, never by inferring the relationship.

A revision that has reached `done` (parent merged) drops off the Review affordance with the parent — the whole card moves to Done together.

### Edge: parent in Review, revision still in Doing

Common case: the operator files R1 while T651 sits in review; R1 is in Backlog/Doing (its own card) while T651 is in Review (its card, no R1 line yet). Once R1's worker pushes and R1 flips to `in_review`, R1's standalone card disappears and the `⟳ R1` line appears under T651. The transition is purely status-driven and needs no special handling.

---

## Design Question 6 — Coordinator System-Prompt Addition

The Boss coordinator session needs to learn the verb exists and when to reach for it, without a keyphrase list (the brief is explicit: trust the coordinator to recognize feedback intent). One paragraph, added to the coordinator's system prompt:

> **Revision tasks.** When the operator gives feedback on a task whose PR is already open and in review — asking to change, add to, or fix something in that work *before it merges* — that is a **revision**, not a new chore. A revision adds a commit to the existing PR rather than opening a new one. Create it with `boss task create-revision --parent <task> --description "<the operator's ask, kept short>"`. Reach for this whenever the operator's intent is "amend the work that produced this open PR" rather than "start something new". Do not use it if the parent has no PR yet, or if the PR is already merged or closed — in those cases a normal `boss task create` (a fresh chore) is correct, and `create-revision` will refuse with a gate error pointing you there. Pass the operator's wording through to `--description` verbatim where it is already concise; summarize only if it is long, because that text is what reviewers see on the kanban.

This teaches recognition (feedback on an in-review PR), the command shape, and the gate boundary, while deferring keyphrase judgment to the model.

---

## Design Question 7 — Effort Classification

### How effort works today

Effort is a marker-based scan of a task's title + description (`effort.rs`, `audit_effort.rs`) — there is **no kind dimension** in the heuristic today, and there is no per-kind effort rule in any `CLAUDE.md` under `tools/boss` (the effort guidance is code, not prose). The scan counts markers to suggest a level (`trivial`/`small`/`medium`/`large`/`max`).

### Options

- **(a) No special handling** — a revision is scanned like any task by its description.
- **(b) Kind-aware default**: `revision` defaults to `small`, then the marker scan can *escalate* (never silently downgrade) if the description carries large-effort markers.
- **(c) Always `small`/`trivial`, no escalation.**

### Discussion

Revisions are usually narrow ("rename the flag", "handle the empty case") — (c)'s instinct is right most of the time but wrong for the occasional "actually, re-architect how this handles concurrency" revision, which is rare but real. (a) ignores the strong prior that revisions are narrow. (b) encodes the prior (default `small`) while letting the existing marker machinery catch the exceptions — the same "default low, escalate on signal" shape the brief suggests.

### Recommendation

**Pick (b).** `create-revision` defaults `--effort` to `small` when the operator does not pass one. The existing marker scan runs on the description and may *raise* the level (large/max markers win over the default) but never lowers it below `small`. Document this as the one kind-specific rule in the effort module: revisions start at `small`, escalation patterns still apply. The operator can always override with explicit `--effort`.

---

## Open Questions (for the human reviewer to land before implementation)

These are crisp decisions with stakes spelled out. Each has a recommended answer; the reviewer can accept or redirect.

### OQ1 — Parent PR merges while a revision is in-flight

A revision worker is editing the parent branch when the parent PR merges out from under it. **Options:** (a) revision auto-blocks and the worker exits cleanly; (b) revision converts to a fresh chore against `main`; (c) surface for human attention and pause.

**Recommended: (a) with attention.** On the dispatch-time gate (Q4) detecting `Merged`, do not spawn — move the revision to `blocked` with `blocked_reason = "parent PR merged before this revision dispatched"` and a `WorkAttentionItem`. If the merge happens *mid-run* (worker already spawned), the completion detector finds the branch merged and the push either fast-forwards a no-op or fails; either way the revision lands in `blocked` + attention rather than silently opening a new PR. (b) is rejected as too clever — converting a "revise this PR" into "new PR against main" changes the operator's intent without consent. The operator can re-target manually (file a chore) after seeing the attention item. *Stakes: pick wrong and a revision either silently spawns a duplicate PR (worst) or auto-creates work the operator didn't ask for.*

### OQ2 — Revision chains and sequence numbering

Can a revision have its own revision (second-pass feedback on R1)? **Recommended: yes**, and the sequence is **chain-root-scoped, creation-ordered** (Q1): a revision-of-R2 is **R3**, not "R2.R1". Rationale: a human reading the parent card wants "this PR has had 3 rounds", and all revisions in a chain target the *same* PR, so a flat R1/R2/R3 across the chain is the truthful count. The parent linkage still records the *immediate* parent (`parent_task_id` points at R2, not the root), so provenance is preserved; only the *display number* is chain-root-scoped. *Stakes: nested numbering (R2.R1) leaks the chain's tree shape into the UI for no operator benefit and complicates the Review-lane rollup.*

### OQ3 — Permission scope / `gh pr create` guard

Trust the prelude, or hard-guard? **Recommended: hard-guard** (Q4) — a per-execution `gh` wrapper that rejects `pr create` for `revision_implementation`. *Stakes: trusting the prelude alone means one confused worker turn can open a duplicate PR, which is exactly the one-PR-per-task invariant ([[feedback_one_pr_per_task]]) this design is the sanctioned exception to — a stray second PR is the most damaging failure mode here.* Reviewer should confirm the worker permission layer can key a rule on execution kind (the engine passes it into the worker env; if not, the guard becomes a small env-conditional in the wrapper).

### OQ4 — Source-B description shape (forward-looking constraint)

When the deferred comment-triage UI creates a revision, what does `--description` look like? It renders on the Review-lane single-line affordance, so it must be terse. **Recommended convention:** `addressing @<author>'s comment on <file>:<line>` (e.g. "addressing @alice's comment on runner.rs:712"), with the full `(repo, pr#, comment-id)` pointer carried in `created_via` (Q1), not in the description. *Stakes: if B writes verbose descriptions, the Review rollup becomes unreadable; setting the constraint now means B is built to it rather than retrofitted.* Per [[feedback_github_is_source_of_truth_for_pr_artifacts]], B stores the pointer, not a mirrored copy of the comment body.

### OQ5 — Cube workspace fallback when the preferred workspace is gone/leased

**Recommended: soft-prefer** (Q3) — `--prefer` the chain root's last workspace for warmth, but fall back to `any_free` (not terminal failure) because the branch state is recoverable from GitHub via `jj git fetch`. Requires the additive `executions.prefer_is_soft` signal so the coordinator's fallback matrix (`coordinator.rs:1801`) treats revisions differently from orphan-resume. *Stakes: the existing matrix fails terminally when a preference can't be honored; applied to revisions that would wedge a revision behind a busy workspace for no reason, since any workspace is correct after a fetch.* Reviewer should confirm the `prefer_is_soft` column is acceptable vs. encoding the policy off `execution.kind` directly (the latter is fewer columns but couples the coordinator's lease logic to a kind string).

### OQ6 — Boss tracking comment on the parent PR

Should the revision worker post `[boss-revision] R<n>: <description>` on the parent PR after pushing? **Recommended: yes** (step 9 of the directive, Q3). It gives reviewers an in-PR breadcrumb for what each new commit was for, which is precisely the context a growing PR loses. *Stakes: low — worst case is mild PR-comment noise; the upside is reviewer clarity.* Reviewer should confirm the comment prefix (`[boss-revision]`) and whether it should be suppressible per-product.

### OQ7 — Does a revision ever reach `done` independently of the parent merging?

The Q4 recommendation ties revision `done` to parent-PR merge. **Open:** is there a case where a revision should be "done" while the parent PR stays open indefinitely (e.g. the operator wants the commit landed but the PR parked)? **Recommended: no** — a revision is `in_review` (commit pushed, rolled up under parent) until the parent merges, then `done`. *Stakes: if revisions could go `done` while the PR stays open, the Review rollup would lose them prematurely and the operator would lose sight of in-flight revision context.* Reviewer confirms.

---

## Schema and Wire Summary

### Column adds

```sql
-- tasks: parent linkage for revisions.
ALTER TABLE tasks ADD COLUMN parent_task_id TEXT;   -- soft FK → tasks.id; NULL
                                                    -- for non-revision rows;
                                                    -- app-enforced NOT NULL when
                                                    -- kind = 'revision'.
CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id ON tasks(parent_task_id);

-- executions: soft-prefer signal for cube lease fallback (OQ5).
ALTER TABLE executions ADD COLUMN prefer_is_soft INTEGER NOT NULL DEFAULT 0;
```

`tasks.kind` gains the value `'revision'` (no enum/CHECK; validation in the application layer, consistent with every other kind). The "kind = revision ⇒ parent_task_id IS NOT NULL" invariant is enforced in `insert_revision_in_tx` and on task update, not by a DB constraint (Q1).

Optional, deferred (flagged, not v1): `tasks.revision_commit_sha TEXT` if per-commit Review-lane links are wanted (Q5); `tasks.revision_source TEXT` if a hard sub-kind enum is ever wanted over `created_via` (Q1).

Migrations follow the `migrate_tasks_*_columns` pattern (`work.rs:8156`): `table_has_column` guard + single `ALTER TABLE … ADD COLUMN`, idempotent, no backfill (existing rows default to `NULL`/`0`).

### Protocol / wire additions

```rust
// protocol/src/wire.rs — mirrors CreateInvestigation (wire.rs:399).
CreateRevision { request_id: String, input: CreateRevisionInput }

// protocol/src/types.rs
pub struct CreateRevisionInput {
    pub parent_task_id: String,          // resolved from --parent selector
    pub description: String,             // the ask; rendered on Review rollup
    pub priority: Option<String>,        // defaults to parent's
    pub effort_level: Option<EffortLevel>, // defaults to 'small' (Q7)
    pub model_override: Option<String>,
    pub force_duplicate: bool,
    pub created_via: Option<String>,     // "operator" (A) or
                                         // "pr-comment:<repo>#<pr>:<cid>" (B)
}

// Task projection gains (mirrored into Models.swift WorkTask):
//   parent_task_id: Option<String>
//   revision_seq:   Option<i64>     // computed R<n>, engine-supplied
//   revision_parent_pr_url: Option<String>  // chain root's pr_url, for the card link
```

The `revision_seq` and `revision_parent_pr_url` fields are **engine-computed** projections, not stored columns — the kanban consumes them and never recomputes the chain (Q5, [[feedback_engine_owns_reconciliation_not_ui]]).

### Engine touch-points (for the follow-up chores to target)

- `work.rs` — `insert_revision_in_tx` (mirror `insert_investigation_in_tx` at `work.rs:7074`); `chain_root(task_id)` helper; `assert_parent_revisable`; revision arm in the dispatch reconcile loop (`work.rs:1622-1656`, dispatch `revision_implementation` independently like `investigation`); extend `mark_chore_pr_merged` to flip in-review revisions to `done`; revision-seq projection.
- `runner.rs` — `revision_implementation` arm in `spawn_prompt` (`runner.rs:700`); `compose_revision_directive` (mirror `compose_investigation_directive` at `runner.rs:809`); add `revision_implementation` to the acceptance `matches!` (`runner.rs:718`).
- `coordinator.rs` — soft-prefer in `lease_workspace_with_fallback` (`coordinator.rs:1817-1840`); populate `preferred_workspace_id` from the chain root's last workspace for revision dispatch.
- `merge_poller.rs` / completion — dispatch-time gate via the existing single-PR probe; revision completion detection (parent head advanced, no new PR).
- `cli/src/main.rs` — `create-revision` verb + `RevisionCreateArgs` (mirror `create-investigation`); gate-error messages (Q2).
- `app-macos` — `WorkTask` fields (`Models.swift`); revision card + Review rollup affordance (`ContentView.swift:689-748`, `:1711-1736`).
- coordinator system prompt — the paragraph from Q6.
- worker permission layer — the `gh pr create` hard-guard for `revision_implementation` (OQ3).

---

## Risks

**R1 — Stray duplicate PR.** A revision worker ignores the directive and runs `gh pr create`, producing a second PR for the same work — the exact violation of [[feedback_one_pr_per_task]] this kind is the sanctioned exception to. *Mitigation:* hard-guard `gh pr create` for `revision_implementation` (OQ3), not prelude trust alone.

**R2 — Editing a merged branch.** The parent merges between dispatch decision and worker push. *Mitigation:* dispatch-time re-poll via the existing poller (Q4) + the merged-mid-flight policy (OQ1); the hard-guard prevents the failure mode from degrading into a new PR.

**R3 — Wrong/cold workspace breaks the branch checkout.** A revision lands in a workspace without the parent branch. *Mitigation:* `jj git fetch` is step 1 of the directive (Q3) — the branch is always recoverable from GitHub; warmth is an optimization only ([[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]]).

**R4 — Soft-prefer regression on orphan-resume.** Adding a soft-prefer path risks loosening the orphan-resume path that legitimately needs a *hard* prefer. *Mitigation:* `prefer_is_soft` defaults to `0`; only `revision_implementation` sets it. Orphan-resume (`work.rs:671`) is untouched.

**R5 — Review rollup gets noisy.** Long revision descriptions (especially from future Source B) make the parent card unreadable. *Mitigation:* hard truncation in the affordance + the terse-description convention (OQ4) baked in before B is built.

**R6 — Sequence number drift.** A computed `R<n>` could surprise if a mid-chain revision is deleted. *Mitigation:* sequence is creation-ordered over surviving revisions (Q1); deleting R1 renumbers R2→R1, which is the intuitive "now there's one revision and it's the first" reading. Stored numbers would have been worse (gaps).

**R7 — Completion detection false-negative.** The detector must distinguish "the parent head advanced because of *my* push" from "advanced because something else pushed". *Mitigation:* v1 confirms the printed URL matches the chain root's PR and the head advanced since spawn; if higher fidelity is needed, capture the pushed SHA (deferred `revision_commit_sha`, Q5) and match on it.

**R8 — Parent linkage outlives the parent.** A parent task is deleted while revisions reference it. *Mitigation:* `parent_task_id` is a soft FK (no cascade), consistent with `project_id`; a revision whose parent is gone is surfaced as a broken-parent attention item rather than crashing a join. Walking to chain root tolerates a missing link.

---

## Phased Implementation Plan

Five child tasks, smallest first, each shippable independently. File these as follow-ups after this design is approved.

1. **Schema + protocol (smallest).** Add `tasks.parent_task_id` + index and `executions.prefer_is_soft` migrations (mirror `migrate_tasks_investigation_doc_columns`). Add `CreateRevisionInput`/`CreateRevision` wire types and the `revision`-kind recognition (no behavior yet). Add `chain_root` helper + unit tests. *Acceptance:* fresh init and migration both yield the new columns; `chain_root` walks chains correctly including the broken-parent case; existing tests green. Ships dark — no kind is dispatchable yet.

2. **CLI `create-revision` + create-time gate.** `RevisionCreateArgs`, `insert_revision_in_tx` (parent-required invariant), `assert_parent_revisable` with the precise gate-error messages (Q2). *Acceptance:* `create-revision --parent <merged>` / `<no-pr>` / `<closed>` each return the right error; `--parent <open>` creates a `revision` row with the parent linkage; revision-of-revision resolves the gate against the chain root.

3. **Dispatch + completion.** `revision_implementation` dispatch arm (`work.rs` reconcile loop); `compose_revision_directive` + spawn arm (`runner.rs`); soft-prefer cube lease (`coordinator.rs`); dispatch-time re-gate via the existing poller probe; completion detection (parent head advanced, no new PR); extend `mark_chore_pr_merged` to flip in-review revisions to `done`. *Acceptance:* end-to-end on a test PR — revision spawns into a (preferably warm) workspace, pushes a commit to the parent branch, opens no new PR, reaches `in_review`; merging the parent flips the revision to `done`. Merged-mid-flight lands the revision in `blocked` + attention (OQ1).

4. **Permission hard-guard + coordinator prompt.** The per-execution `gh pr create` rejection for `revision_implementation` (OQ3); the coordinator system-prompt paragraph (Q6). *Acceptance:* a `revision_implementation` worker attempting `gh pr create` is rejected with the contract message; a normal worker is unaffected; the coordinator, given simulated operator feedback on an in-review PR, reaches for `create-revision`.

5. **Kanban chrome.** `WorkTask` fields (`parentTaskId`, `revisionSeq`, `revisionParentPrUrl`); distinct revision card in Backlog/Doing; per-revision rollup line on the parent's Review-lane card (Q5). *Acceptance:* a revision shows a distinct `⟳ R<n>` card in Backlog/Doing and disappears into a single rollup line under the parent in Review; numbering is chain-root-scoped; all UI text uses "R<n>"/"Revision n", never "Friendly ID" ([[feedback_no_friendly_id_in_ui]]).

A sixth, explicitly out of scope here: the Source-B comment-triage UI, which becomes a thin producer calling `CreateRevision` with a `created_via` pointer (Q1, OQ4) once this mechanism lands.

---

## Out of Scope

- The Source-B GitHub PR-review-comment triage UI (separate effort; this is its substrate).
- Auto-applying review comments without a human gate.
- Cross-PR revision (one revision = one commit to one PR).
- Auto-rebasing revisions onto a moved-on main (normal conflict-resolution flow applies, or fail loud).
- Rewriting the parent task's brief.
- A dedicated "revisions" kanban column.
- Auto-merging the parent PR after N revisions (always human-merged).
- Per-commit Review-lane links and a hard `revision_source` sub-kind enum (both deferred, flagged in Q1/Q5).

## Related

- [[engine_owns_reconciliation_not_ui]] / [[feedback_engine_owns_reconciliation_not_ui]] — parent↔revision linkage and the `R<n>` sequence are engine-owned, UI-rendered.
- [[feedback_one_pr_per_task]] — revisions are the explicit exception: one PR per parent chain + N commits via revisions.
- [[feedback_github_is_source_of_truth_for_pr_artifacts]] — Source B stores the `(repo, pr#, comment-id)` pointer (in `created_via`), not a mirrored comment body.
- [[feedback_no_friendly_id_in_ui]] — UI uses "R1" / "Revision 1", never "Friendly ID".
- [[feedback_cube_workspaces_are_warmed_caches_no_chore_stickiness]] — `--prefer` is warmth-only; `jj git fetch` makes any workspace correct.
- T641 (investigation kind) — the parallel "add a new kind" template this design mirrors throughout (migration, CLI verb, dispatch arm, spawn directive, kanban affordance).
- T653 (engine-isolation) — sibling concern in the same engine surface; not a dependency.
- `design-producing-tasks.md`, `project-design-doc-pointer.md` — prior art for the kind/schema decisions reused in Q1.
