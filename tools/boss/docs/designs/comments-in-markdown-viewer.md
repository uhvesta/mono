# Boss: Comments in the Markdown Viewer

Design doc for an in-app comment system that lets the user highlight a
region of any rendered markdown surface in Boss — work-item
descriptions, the popped-out design-doc viewer, anything else that
renders markdown — attach a comment, see it in a sidebar, and (via a
"magic wand" affordance) dispatch a doc-editing agent that acts on the
comment.

## Goals

- Any markdown surface the macOS app renders can be commented on. The
  primary surfaces in scope today are the expanded work-item description
  (`MarkdownViewerView` in `tools/boss/app-macos/Sources/DesignsView.swift`)
  and the design-doc viewer (`DesignRendererView`); the contract is
  written so that adding the affordance to a future surface is a
  one-line change.
- Selecting a span of rendered text exposes an authoring affordance.
  Hitting enter creates a comment. The commented region stays visually
  highlighted in the doc.
- Comments persist in the engine. They survive app restarts, doc
  edits, and view re-opens.
- Comments stay anchored to the originally selected text even when the
  underlying markdown is edited (lines added, paragraphs reflowed,
  words inserted near the anchor). Line-number-only anchoring is
  explicitly rejected as too fragile.
- A right-side sidebar lists comments for the open doc with author,
  timestamp, snippet, dismiss action, and a magic-wand button. The
  doc highlight and the sidebar entry are bidirectionally linked:
  hovering one highlights the other; clicking a sidebar entry scrolls
  the doc.
- The magic wand dispatches a doc-editing agent. Two dispatch paths:
  - **Engine-owned doc** (e.g. a work-item description): a
    *specialised, isolated Claude instance* with a deliberately narrow
    tool surface — see [Magic-wand sandbox](#magic-wand-sandbox).
  - **PR-backed doc** (e.g. a design doc on a `boss/exec_*` branch):
    the existing Boss chore-worker path, given a directive that
    encodes the comment's intent against the PR's branch.
- The interaction model is validated end-to-end on real docs *before*
  the engine learns to persist comments or dispatch agents. Phase 1
  ships UI only.

## Non-goals

- Commenting on PR diff views, chat panes, or the kanban-card preview
  text. These surfaces have their own UX questions and are out of
  scope.
- Threaded replies (replies-to-replies, nesting). Single-level only
  in v1; thread later if real usage demands it.
- A general "show me previous versions of this work-item description"
  feature. Comment-anchor versioning (the narrow CAS needed to keep
  the magic wand safe) is in scope; full description history is a
  separate project (see [Versioning](#versioning)).
- Inline diff rendering of the magic-wand result. v1 shows a
  side-by-side preview built from the returned markdown; a real
  intra-paragraph diff renderer can come later.
- Multi-user permissions. Boss is a single-user tool today; anyone
  can dismiss any comment. The schema records `author` so a future
  ACL layer has a hook to grow into.
- Realtime collaboration (live presence, OT/CRDT). Two simultaneous
  comments at the same anchor just produce two separate comment rows.
- Auto-applying magic-wand output. Every magic-wand result lands in a
  *preview* state and requires explicit user accept before the source
  is overwritten.

## Alternatives considered

### Alternative A: GitHub-style line-anchored comments

Anchor every comment to a `(file_path, line_number, column_range)`
tuple, the way GitHub PR review comments do.

- **Pros**: trivial to serialize; matches an existing mental model;
  the line-range is what `gh pr comment` uses, so a future export-to-
  GitHub path would be easy.
- **Cons**: any non-trivial edit invalidates every comment below the
  edit point. The motivating use case for this feature is exactly the
  case where the doc keeps changing — the human comments, the magic
  wand or the worker edits, the human re-reads. Line-anchored
  comments would re-attach to the wrong text on every edit. The
  problem is well-studied: the W3C Web Annotation Data Model exists
  specifically because line-anchoring fails on edited docs.
- Rejected.

### Alternative B: Per-doc inline `<!-- comment: ... -->` markers stored in the markdown source

Store comments as HTML-comment-shaped markers embedded in the markdown
itself.

- **Pros**: zero new schema. Comments travel with the doc trivially
  (a PR-backed doc literally carries its comments in git).
- **Cons**: visible to anyone editing the raw markdown — including
  workers, which would pollute their prompts. Comments aren't really
  the doc's content; mixing them in violates the separation between
  artifact and conversation. Dismissed comments leave residue.
  Anchoring is still required (a marker placed mid-paragraph still
  drifts when the surrounding text is rewritten). Worst of all: the
  magic-wand instance would see its own comment in the markdown it's
  asked to edit, creating a self-referential mess.
- Rejected.

### Alternative C: W3C Web Annotation `TextQuoteSelector` anchoring in the engine, comments stored as engine rows

What this design recommends. Comments live in a new `work_comments`
table keyed to the artifact (work-item id, or PR-doc ref). Each
comment carries a `TextQuoteSelector`-shaped anchor (`exact`,
`prefix`, `suffix`). The renderer re-resolves the anchor against the
current doc text on every load; resilience falls out of the selector
model. Dispatch routes through the engine's existing execution path
(or a new sibling for the specialised-Claude case).

- **Pros**: anchoring is robust to most realistic edits because the
  prefix/suffix context disambiguates even when the exact text recurs.
  Comments are first-class engine objects so they show up in the
  subscription stream, survive crashes, and feed naturally into the
  existing attention/inbox plumbing. The magic-wand path becomes a
  small addition rather than a rewrite of the dispatch surface.
- **Cons**: anchoring isn't free — the renderer has to do a quote
  match on every load, and the engine has to store ~80–200 bytes of
  selector per comment instead of a 4-byte line number. The renderer
  needs to walk the rendered AST to map matched text back to layout
  coordinates. Both costs are bounded and well-understood.
- **Chosen.** Detailed below.

## Chosen approach

### Phasing

The user has called for the UI interaction model to be validated
*before* the agent path is built. The design adopts that explicitly:

- **Phase 1 — UI shell.** Selection → comment author → sidebar render
  → highlight → dismiss. **No engine persistence** (comments live in
  in-memory state on the viewer window, lost on close). **No magic
  wand.** Anchoring is line+offset since comments don't survive.
  Goal: prove the interaction is good on real docs before paying the
  cost of persistence.
- **Phase 2 — Persistence + resilient anchoring.** New engine schema,
  RPCs, subscription topic, re-anchoring on load. W3C-style
  `TextQuoteSelector`. Soft-dismiss with a history surface.
- **Phase 3 — Magic wand (engine-owned docs).** The specialised
  isolated-Claude dispatch path. Comment-anchor CAS for safety.
- **Phase 4 — Magic wand (PR-backed docs).** Routes to a Boss chore
  worker against the PR's branch.

Each phase is filed as its own implementation task under the parent
project, with Phase N+1 depending on Phase N via the dependency graph
(`work-dependencies.md`). Phase 1's deliverable is a working
prototype the user can drive against the existing markdown viewer;
phases 2–4 each ship their slice independently.

### Anchoring model

Comments anchor with a [W3C Web Annotation Data Model][wadm]
`TextQuoteSelector`, serialised inline on the comment row:

```json
{
  "type": "TextQuoteSelector",
  "exact": "the rendered markdown source already pushes commented spans",
  "prefix": "Each comment carries a `TextQuoteSelector`-shaped anchor (`exact`, `prefix`, `suffix`). ",
  "suffix": " through to the macOS app via the existing subscription"
}
```

[wadm]: https://www.w3.org/TR/annotation-model/#text-quote-selector

The anchor's three fields are **strings taken from the rendered
plain-text projection of the markdown** (not the raw markdown source).
Rationale: the user selects on rendered text, so what they see is what
gets stored. The renderer maintains a `[(plainTextRange, sourceRange,
nodeId)]` mapping (built once per render pass) so the engine never has
to know about layout, and the macOS app never has to round-trip raw
markdown back through the parser to compute selection.

**Prefix/suffix length.** Fixed 64 characters each, trimmed at word
boundaries where possible. 64 is enough to disambiguate within any
realistic Boss-sized doc (work-item descriptions are typically
<10kB, design docs <100kB) and short enough that prefix/suffix
collisions on edited docs are rare. The schema permits longer values
for callers that want them — a future "comment on a function name in
a code block" affordance might need more context — but the default
authoring path writes 64.

**Re-anchoring on load.** When the renderer loads a doc with comments
attached, it runs each anchor through this resolution:

1. **Exact match** — search the rendered plain text for `prefix + exact + suffix`
   verbatim. If found exactly once, the comment is anchored.
2. **Fuzzy match** — if the exact form is not found, attempt a
   fuzzy match with [`fastdiff`][fastdiff]-style scoring across
   sliding windows. A comment is considered re-anchored if the best
   match scores ≥0.8 against the original `exact + prefix + suffix`
   *and* is uniquely the best (the second-best match scores <0.7).
3. **Orphan** — if neither exact nor fuzzy resolution succeeds, the
   comment is *orphaned*: it still appears in the sidebar with an
   "anchor lost" badge and the original snippet, but does not paint
   a highlight in the doc. Orphans can be dismissed normally; a
   future "re-attach to current selection" affordance is a natural
   follow-up.

[fastdiff]: https://docs.rs/fastdiff/

The fuzzy threshold is tunable per-product via the existing engine
config surface; 0.8 / 0.7 are starting values borrowed from
[Hypothes.is's annotation re-anchoring][hypo-anchor] which has the
most real-world miles on this exact problem.

[hypo-anchor]: https://web.hypothes.is/blog/fuzzy-anchoring/

**Why not `RangeSelector` or DOM-path-style anchors.** The W3C model
also defines `RangeSelector` (start/end paths into a structured tree)
and `XPathSelector`. Both bind to the *rendered DOM*, which means
any structural change to the document (a new heading inserted before
the anchor, a paragraph split, a list re-bulleted) breaks them. A
plain `TextQuoteSelector` is content-addressed: it survives any edit
that doesn't touch the immediate text around the anchor. For Boss's
needs that's strictly better.

### Engine schema

One new table:

```sql
CREATE TABLE work_comments (
  id              TEXT PRIMARY KEY,         -- e.g. "comment_18b...ef"
  artifact_kind   TEXT NOT NULL,            -- 'work_item' | 'pr_doc'
  artifact_id     TEXT NOT NULL,            -- work_items.id, OR
                                            -- "pr_doc:<repo>:<branch>:<path>"
  doc_version     TEXT NOT NULL,            -- SHA-256 of the doc text
                                            -- the comment was authored against
  anchor_json     TEXT NOT NULL,            -- {exact, prefix, suffix}
  body            TEXT NOT NULL,            -- the comment text
  author          TEXT NOT NULL,            -- 'user:<email>' or
                                            -- 'magic_wand:<comment_id>'
  status          TEXT NOT NULL,            -- 'active' | 'dismissed' |
                                            -- 'orphaned' | 'resolved'
  status_actor    TEXT,                     -- who flipped status last
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  dismissed_at    INTEGER
);
CREATE INDEX work_comments_by_artifact ON
  work_comments(artifact_kind, artifact_id, status);
```

Notes:

- `artifact_kind = 'work_item'` is the engine-owned case: a comment
  on a `tasks.description` or a project's description. `artifact_id`
  is the work-item id.
- `artifact_kind = 'pr_doc'` covers the PR-backed case: a comment on
  a markdown file under a PR's `boss/exec_*` branch. `artifact_id`
  is the synthetic composite key — repo, branch, path — that uniquely
  identifies the file. This means a single design doc on a PR carries
  its comments as long as the PR's branch lives.
- `doc_version` is the SHA-256 of the *plain-text projection* of the
  doc at authoring time, not the raw markdown SHA. The plain-text
  projection is what the anchor lives in, so this is the right
  invariant for CAS (Q below).
- `status = 'orphaned'` is a derived state — the engine doesn't
  flip rows to orphaned itself; instead the renderer reports orphans
  to the engine as a side-effect of the load, and the engine stores
  the flip for surfacing in the sidebar. This keeps the engine from
  having to itself parse markdown.
- `status = 'resolved'` is the soft-dismiss outcome: the comment is
  hidden from the active sidebar but remains in the history surface.
  Hard delete is not exposed in v1.
- Author is `user:<email>` for human-authored comments. The
  magic-wand path may produce *replies* in v2; reserved syntax is
  `magic_wand:<comment_id>` to keep them visually distinct.

The work-items themselves do not gain any columns; comments are
strictly auxiliary state. This means a work item's PR detection,
ready-to-spawn check, attention items, and dispatch flow are
unaffected by the addition of comments.

### Doc-version invariant

`doc_version` is a SHA-256 of the doc's *current plain-text
projection* at authoring time. The engine computes it; the renderer
provides the plain text inline in the create RPC (so the engine and
renderer agree on the input).

The version is used in two places:

1. **Magic-wand CAS** (Phase 3 / 4) — when the magic-wand result is
   ready to apply, the engine compares the doc's current
   plain-text hash against the comment's `doc_version`. Mismatch
   triggers a conflict surface ("the doc changed since you
   commented; reload and re-attach"), not a silent overwrite.
2. **Anchor-resolution diagnostic** — the renderer can report "the
   doc version stored on this comment doesn't match the version I
   loaded" as a non-fatal warning; orphans become expected when this
   happens. Useful telemetry.

No general "history of doc versions" is stored. The version field is
opaque, used only for equality comparison.

### RPCs and subscription topics

New RPCs on the engine, served on the existing control socket and
following the conventions in
[`work-subscriptions`](work-subscriptions.md) and
[`engine-app-rpc`](engine-app-rpc.md):

- `comments_create(artifact_kind, artifact_id, doc_version,
  anchor, body) → Comment` — creates an `active` comment. Returns
  the row.
- `comments_list(artifact_kind, artifact_id, include_dismissed?)
  → [Comment]` — fetches all comments for an artifact. Default
  excludes `dismissed` and `resolved`.
- `comments_update_anchor(comment_id, new_anchor, new_doc_version)
  → Comment` — called by the renderer after re-anchoring on a fresh
  load, when the anchor resolved fuzzy-match rather than exact-match.
  Lets the engine learn the new shape so subsequent loads exact-match.
- `comments_set_status(comment_id, status, actor) → Comment` —
  dismiss / resolve / re-activate transitions. Resolves are
  recoverable (`active` accepted as a transition); hard delete is
  not exposed.
- `comments_dispatch_magic_wand(comment_id) → DispatchHandle` — the
  magic-wand entry point. Returns an execution-handle the UI can
  subscribe to for status.
- `comments_fetch_with_doc_version(artifact_kind, artifact_id) →
  {doc_text, doc_version, comments}` — convenience: one round-trip
  to load both the doc text the engine has on record (for
  work-item descriptions) and its current comments. PR-backed docs
  return `doc_text = null` because the source-of-truth is the git
  branch, not the engine.

New subscription topic, following the invalidation-not-patch shape
used everywhere else:

- `comments.artifact.<artifact_kind>:<artifact_id>` — fires
  whenever any comment row on the artifact changes. Clients
  refetch via `comments_list`.

Auth: `comments_*` RPCs are user-tier — both the SwiftUI app and
worker sessions can read and write. The `comments_dispatch_magic_wand`
RPC is gated to the Boss-session subtree (it spawns an agent;
workers must not be able to trigger that themselves). See
[control-plane tiers](main.md#control-plane).

### Comments on PR-backed docs

When the doc being commented on is a markdown file under an active
PR's branch (the design-doc viewer's primary case), the
`artifact_id` is `"pr_doc:<repo_remote_url>:<branch>:<path>"`. The
engine stores comments against that key directly.

**Migration when a doc graduates from work-item description to
PR.** When a design-task worker pushes its doc and a PR appears,
existing comments attached to the work item's description should
travel with it. The migration is a one-time operation triggered by
`DesignDetector`'s `in_review` transition: every active comment
with `artifact_kind = 'work_item'` and `artifact_id = <task_id>`
is re-keyed to the new `pr_doc:<repo>:<branch>:<path>` artifact and
re-anchored against the PR's doc text. Re-anchoring uses the
existing fuzzy resolution; comments that can't re-anchor become
orphans on the PR-doc side. The original work-item comments stay
around at status `resolved` so the trail is visible.

**Branch lifecycle.** When the PR's branch is deleted (merge,
abandon), the `pr_doc:*` artifact id becomes stale. Comments on
deleted branches transition to `orphaned` automatically (background
sweep); the sidebar groups them under the artifact for archival
viewing. They are never deleted server-side in v1.

### macOS app architecture

A small new module under `tools/boss/app-macos/Sources/Comments/`
hosts the comment overlay. Pieces:

1. **`CommentLayer`** — a `ViewModifier` applied to a
   `StructuredText` view. It owns:
   - The `[Comment]` for the current artifact (loaded from engine).
   - A `SelectionTracker` observing the underlying selection state.
     SwiftUI's `.textSelection(.enabled)` exposes selections via
     `NSTextView`-bridged callbacks on macOS; the tracker reflects
     them into a SwiftUI `@StateObject` we can drive UI from.
   - The rendered-plain-text → source-range → AST-node mapping
     emitted by the markdown renderer (one walk per load).
   - A computed `[(commentId, NSRange in plain text)]` derived by
     resolving each comment's anchor against the plain-text
     projection.

2. **`CommentPopover`** — a transient SwiftUI popover that appears
   anchored to the current selection's bounding rect. Contains the
   author field, the body editor, and a "Comment" submit button
   (cmd-return). Submits via `comments_create`.

3. **`CommentHighlightOverlay`** — a `Canvas`-backed overlay sibling
   to the `StructuredText` view that paints subtle background tints
   for each `(commentId, NSRange)` pair. Uses
   `NSLayoutManager.boundingRect(forGlyphRange:in:)` via a small AppKit
   bridge to translate ranges into rects. Hover state on a highlight
   bumps the tint and notifies the sidebar; click scrolls the sidebar
   entry into view.

4. **`CommentSidebar`** — a fixed-280pt right-side panel that
   `MarkdownViewerView` and `DesignRendererView` opt into via a new
   `.commentSidebar(artifact:)` modifier. Lists comments in document
   order (sorted by anchor position). Each row shows author,
   timestamp, snippet, the comment body, and three controls: dismiss,
   resolve, magic-wand. Hovering a row scrolls and tints the
   corresponding doc highlight (bidirectional cursor).

5. **`MagicWandResultSheet`** — modal sheet shown when a magic-wand
   dispatch completes. Renders the *current* doc and the *proposed*
   doc side-by-side via two `StructuredText` views, with a single
   "Apply" / "Discard" pair below. v1 does not implement an
   intra-paragraph diff; the user is reading two renders of the
   markdown and judging by eye. A real diff renderer is on the
   post-v1 list.

The sidebar appears only when the artifact has at least one comment
*or* the user has explicitly toggled it on; toggle state is per-view,
not persisted. This keeps the chrome out of the way of users who
aren't using the feature.

### Magic-wand dispatch

Two routes, decided at dispatch time by the artifact kind.

#### Engine-owned doc → specialised isolated Claude

For `artifact_kind = 'work_item'` (the doc lives in the engine's
work-item description, not a PR branch), the engine spawns a
**specialised Claude instance** — distinct from a Boss worker.

**Where it runs.** A new engine module
`tools/boss/engine/src/magic_wand.rs` makes a one-shot
`messages.create` call against the Anthropic API directly (via the
existing `anthropic-sdk` crate — same SDK Boss uses for its other
direct-API calls). **No Claude Agent SDK, no tools, no system
prompt beyond the inlined instructions.** The prompt is:

```text
You are editing a markdown document. The user has highlighted a
section and left a comment. Apply their intent to the document and
return the entire updated markdown verbatim.

Document:
```markdown
<doc_text>
```

Highlighted section:
> <anchor.exact>

Comment:
> <comment.body>

Respond with only the updated markdown. Do not include any
explanation, header, or trailing prose.
```

**No tool surface.** The model's response is the entire updated
markdown as a single message. No `Bash`, no `Read`, no `Edit`, no
`WebFetch`, no MCP — none of these tools are passed to the call.
The instance literally cannot do anything except return text.

**Sandboxing properties this gives us:**

- No filesystem access of any kind.
- No environment access.
- No way to read or write outside the prompt/response.
- No conversation memory between invocations (one-shot).
- A bounded blast radius: the worst the model can do is return
  garbage markdown; the diff sanity check below catches obviously
  broken responses before they reach the user.

**Validation before showing the preview:**

1. **Length sanity** — the returned markdown must be within
   [0.25×, 4×] the source length. Outside that envelope: surface an
   error and discard.
2. **Diff sanity** — compute the line-diff between source and
   returned text; reject if >60% of lines changed. (Tunable; the
   point is to catch "the model rewrote the whole doc" failures.)
3. **Anchor preservation** — the source's `anchor.exact` text need
   not survive verbatim in the result (the whole point of the magic
   wand is sometimes to edit the highlighted section). But if the
   result eliminates the section the comment was about *and* makes
   wholesale changes elsewhere too, surface a warning in the
   preview. The user decides.

Validations 1 and 2 are hard rejects; 3 is a warning displayed in the
preview sheet.

**Billing and observability.** Magic-wand calls run against a
dedicated Anthropic API key configured via the existing engine env
surface (`BOSS_MAGIC_WAND_API_KEY`, falls back to the standard
`ANTHROPIC_API_KEY` if unset — but the user can configure two keys
to bucket spend separately). Token counts are recorded against a new
`magic_wand_dispatches` table for observability:

```sql
CREATE TABLE magic_wand_dispatches (
  id            TEXT PRIMARY KEY,
  comment_id    TEXT NOT NULL REFERENCES work_comments(id),
  artifact_kind TEXT NOT NULL,
  artifact_id   TEXT NOT NULL,
  doc_version   TEXT NOT NULL,            -- the version CAS'd on apply
  status        TEXT NOT NULL,            -- 'in_flight' | 'returned' |
                                          -- 'applied' | 'discarded' |
                                          -- 'conflict' | 'failed'
  input_tokens  INTEGER,
  output_tokens INTEGER,
  result_md     TEXT,                     -- the returned markdown
                                          -- (null on failure)
  error_kind    TEXT,
  created_at    INTEGER NOT NULL,
  resolved_at   INTEGER
);
```

This table is the audit trail for "what did the magic wand do." It
also feeds a future per-comment history surface.

**Apply step.** When the user clicks "Apply" in the preview sheet,
the engine compares the current `work_items.description`'s
plain-text-projection SHA against the comment's `doc_version`. On
match: the description is overwritten with the returned markdown,
the dispatch row transitions to `applied`, the comment transitions
to `resolved`. On mismatch: the dispatch row transitions to
`conflict`, the comment stays `active`, and the preview sheet
shows a "the doc changed since you commented" reload affordance.

#### PR-backed doc → Boss chore worker

For `artifact_kind = 'pr_doc'` (the doc lives on a PR branch), the
engine instead dispatches a regular Boss chore worker.

**Mechanism.** The engine creates a chore via the existing
`create_chore` RPC with `repo_remote_url` and a synthetic title
("Address comment on `<path>`: `<short_quote>`"). The chore's
description (rendered into the worker's CLAUDE.md by the existing
runner) encodes the comment intent:

```text
A reviewer left a comment on this PR's design doc.

File: <path>
Branch: <branch>
PR: <pr_url>

Quoted section:
> <anchor.exact>

Comment:
> <comment.body>

Please update the file accordingly and push to the existing PR
branch. Do not open a new PR; this branch already has one.
```

The worker uses the existing PR-resume mechanism: the runner
notices the branch already has an open PR and resumes the
conversation on that branch rather than starting fresh. The
worker's behaviour is then identical to addressing a GitHub review
comment.

**Why not the specialised path here too.** Two reasons. First, a
PR-backed doc is, by definition, a doc whose changes go through
review — the human is already going to see the diff on GitHub
before merge, so the safety case for the specialised
no-tools instance is weaker. Second, design docs and PR-tracked
markdown often live next to code (a worker's CLAUDE.md, a config
file, a snippet of bazel BUILD that the doc references) that the
worker may need to read to understand the comment. The specialised
instance, by design, can't do that.

**Identity.** The dispatched chore is owned by a synthetic actor
`comment_dispatch:<comment_id>` for audit-log purposes. The chore
appears in the kanban under the parent product like any other; the
sidebar's "the magic wand dispatched a worker" row links to the
chore card.

### Versioning

The narrow scope picked here is **comment-anchor CAS** only.

- Each comment records the `doc_version` it was authored against.
- The magic-wand apply step compares that version against the doc's
  current plain-text SHA. Mismatch → conflict, no overwrite.
- That's it. No history, no rollback, no diff-view of past versions.

Broader description history — "show me the work-item description as
it was last Tuesday" — is a separate project. The user has flagged it
as a possible decompose target; it remains out of scope here and is
filed as a follow-up project (`Work-item description history`) for
the human to schedule independently. The choice is deliberate:
introducing a general history mechanism alongside the comment system
would couple two features whose UX and storage shapes are different.

The user's stated phasing note — "rudimentary versioning of work item
comments" — fits this CAS scope exactly. Anything broader is a
separate project.

### Phase-N implementation breakdown

Each phase is one implementation task under this project, with the
phase ordering encoded as dependencies in the task graph.

**Phase 1: UI shell.**

- New `tools/boss/app-macos/Sources/Comments/` module.
- `CommentLayer` + `CommentPopover` + `CommentHighlightOverlay` +
  `CommentSidebar` over an in-memory `[Comment]` array on
  `MarkdownViewerView` and `DesignRendererView`.
- Selection → comment → sidebar → highlight → dismiss flow.
- Naive `(line, offset, length)` anchoring (we throw it away on
  view close).
- No engine RPCs.
- Acceptance: user can open any markdown viewer, select text,
  attach a comment, see it in the sidebar, dismiss it. Closing
  the window loses comments — acceptable for this phase.

**Phase 2: Persistence + resilient anchoring.**

- `work_comments` table migration in the engine.
- `comments_*` RPCs and `comments.artifact.*` subscription topic.
- Renderer plumbing for the plain-text-projection mapping and the
  `TextQuoteSelector` resolver (exact + fuzzy + orphan).
- `comments_update_anchor` callback for fuzzy re-resolves.
- Soft-dismiss with a "show resolved" toggle in the sidebar.
- Migration on `DesignDetector` `in_review` to re-key work-item
  comments to `pr_doc:*` artifacts.
- Acceptance: comments survive app restart and doc edits;
  fuzzy-re-anchored comments update the engine; dismiss flows
  through soft-resolve.

**Phase 3: Magic wand (engine-owned docs).**

- `magic_wand_dispatches` table.
- `tools/boss/engine/src/magic_wand.rs` — the specialised
  one-shot Claude call with no tools.
- `comments_dispatch_magic_wand` RPC, gated to Boss-session subtree
  auth tier.
- `MagicWandResultSheet` side-by-side preview + apply/discard.
- Doc-version CAS on apply.
- Acceptance: clicking magic-wand on a comment against a work-item
  description produces a preview sheet within ~30s; applying
  overwrites the description; rejecting closes the sheet cleanly;
  CAS conflicts surface a reload affordance.

**Phase 4: Magic wand (PR-backed docs).**

- Dispatch-branch logic in `comments_dispatch_magic_wand` for
  `artifact_kind = 'pr_doc'`.
- Synthetic chore creation with the comment-intent directive.
- Hook into the existing PR-resume mechanism (worker resumes the
  existing branch's conversation, doesn't open a new PR).
- Audit-log linkage: the dispatch row references the spawned chore
  id; the chore is owned by `comment_dispatch:<comment_id>`.
- Acceptance: clicking magic-wand on a comment against a PR-tracked
  design doc spawns a worker against the PR's branch, the worker
  pushes a commit addressing the comment, the comment transitions
  to `resolved` when the worker reports it's done.

The four phases are filed as four implementation tasks under this
project. Phase 1's task is `independent`; phases 2, 3, 4 each depend
on the previous via the dependency graph. This way the human can
pause after any phase and assess before committing to the next slice.

## Open questions (answers)

The brief raised a set of open questions. Resolved here, with the
reasoning visible so the human can override on review.

- **Anchoring detail.**
  - Prefix/suffix length: **64 chars each**, trimmed at word
    boundaries. Rationale above.
  - Fuzzy threshold: **≥0.8 score** for the match, **<0.7** for the
    second-best (uniqueness). Starting values from Hypothes.is;
    tunable in engine config.
  - Anchor's containing element deleted entirely: **orphan with
    "anchor lost" badge** in the sidebar, no doc highlight. Manual
    re-attach is a post-v1 affordance.
- **Sidebar layout.**
  - Fixed 280pt width, right side.
  - Appears only when comments exist on the artifact or the user
    explicitly toggles it. Toggle state per-view, not persisted.
  - Does not auto-scroll to follow viewport in v1; the bidirectional
    hover cursor is the navigation primitive. Auto-scroll-on-scroll
    is a post-v1 consideration.
- **Comment threading.** Single-level only in v1. Add threading
  later if real usage demands it.
- **Cross-doc comments.** Work-item-description comments migrate to
  the PR's `pr_doc:*` artifact on `in_review` transition, via the
  re-anchoring path. Originals stay at `status = 'resolved'` for
  the trail. Detailed in [Comments on PR-backed docs](#comments-on-pr-backed-docs).
- **Magic-wand result UX.** Side-by-side preview with explicit
  Apply / Discard. Strongly recommended over one-click-apply for
  v1; one-click is a possible future toggle once the failure modes
  are well understood.
- **Concurrent commenters.** Both comments attach as separate rows;
  the sidebar lists both in document order. No locking, no merge.
  (Single-user system today; this is forward-looking only.)
- **Permissions.** Anyone can dismiss any comment. The `author`
  column records the trail; a future ACL layer can grow into the
  `status_actor` field.
- **Magic-wand instance identity.** Engine-owned-doc dispatches
  attribute the edit to `magic_wand:<comment_id>` on the
  `magic_wand_dispatches` row. PR-backed dispatches attribute the
  spawned chore to `comment_dispatch:<comment_id>` on the
  worker-side audit log.

### Constraint compliance

- **Engine owns reconciliation; UI is a thin client.** Comments
  live in the engine. The renderer reads them, paints them, and
  reports anchor-resolution outcomes; it does not own truth.
- **Use `T<n>` / `P<n>` short-id forms** — comments use prefix
  `cmt_` internally (no short-id form in scope for v1, since
  comments are scoped to the doc they're attached to and don't
  need to be referenced from a CLI). No `C<n>` form is introduced.
- **No invented CLI verbs.** This design adds RPCs but not CLI
  verbs. A future `boss comment list <task-id>` is plausible but
  out of scope here.
- **Specialised-Claude path: in-band or out-of-band?** It runs
  **in-band**, in the engine, via a direct one-shot
  `messages.create` against a separate API key (or budget bucket).
  Observability lands in the new `magic_wand_dispatches` table.
  Decision rationale: keeping it in the engine keeps reconciliation
  simple (same SQLite, same subscription stream) and the
  no-tools-no-system-prompt sandboxing is strong enough that a
  separate runtime would not materially raise the security bar.

## Risks and open questions

- **Risk: anchoring on rendered plain text means the renderer is in
  the trust path for selectors.** If the renderer's plain-text
  projection ever drifts between versions (e.g. a future Textual
  upgrade changes how a code-block fence is rendered), existing
  anchors may all silently start orphaning. *Mitigation*: the
  `doc_version` field will catch coarse drift (the hash changes when
  the projection changes), and the renderer can emit a "projection
  algorithm version" header alongside the plain text so the engine
  knows to re-anchor everything once on a renderer upgrade. Worth
  spec'ing a `plain_text_projection_version` field; deferred to
  Phase 2 implementation.
- **Risk: fuzzy re-anchor false positives.** A 0.8 threshold will
  occasionally re-anchor a comment to the wrong piece of text on
  heavily edited docs. *Mitigation*: the fuzzy outcome is recorded
  on `comments_update_anchor` so the user sees "this comment
  re-anchored fuzzily" in the sidebar (e.g. a small ⚠ glyph) and
  can sanity-check.
- **Risk: magic-wand instance returning subtly broken markdown
  (mis-indented code, dropped fence).** The diff-sanity check
  rejects gross failures, but subtle ones reach the preview sheet
  and rely on the human to spot. *Mitigation*: the side-by-side
  preview is the explicit checkpoint; the human is in the loop.
  If we see this fail in practice, add a "the result fails to
  parse as markdown" reject (parse with Textual's underlying parser
  before showing the sheet).
- **Risk: cost / token budget on magic-wand calls.** Long docs
  consume non-trivial input tokens, and a chatty user can ring up
  many dispatches. *Mitigation*: separate API key allows budget
  bucketing; the `magic_wand_dispatches` table makes per-comment
  cost trivially reportable. A per-day budget cap is a v2 add if
  needed.
- **Open question for human review: should the specialised-Claude
  path run a non-streaming or streaming call?** Streaming would
  let the preview sheet render progressively; non-streaming is
  simpler and gets validated-or-rejected as one atomic result.
  Recommend non-streaming for v1; revisit if latency feels bad.
- **Open question: do orphaned comments contribute to the sidebar's
  comment count, or render in a collapsed "lost anchors" group?**
  Recommend collapsed group; keeps the main sidebar clean.

## Follow-up implementation tasks

Once this design is approved, file the four implementation tasks
sketched in [Phase-N breakdown](#phase-n-implementation-breakdown) as
dependent siblings under this project (Phase 1 independent; Phases 2,
3, 4 each depending on the previous).

Also file the separate project **`Work-item description history`** —
distinct from this work, scoped to a general "show me prior versions
of a description" feature — for the human to schedule independently.

## Related designs

- [`markdown-renderer-migration`](markdown-renderer-migration.md) —
  the renderer this overlay attaches to.
- [`design-producing-tasks`](design-producing-tasks.md) — design
  doc lifecycle, including the `in_review` transition that triggers
  the work-item→pr_doc comment migration.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) —
  how a project's design doc is located, used to resolve the
  artifact id for `pr_doc:*` comments.
- [`work-execution`](work-execution.md) — the execution / run /
  attention plumbing the magic-wand-via-chore path leans on.
- [`engine-app-rpc`](engine-app-rpc.md) — RPC conventions the new
  `comments_*` calls follow.
- [`work-subscriptions`](work-subscriptions.md) — the topic shape
  the new `comments.artifact.*` topic follows.
