# Boss: Notification Near-Duplicate Reconciliation + Scoring

## Problem

When several agents independently flag the same thing, each one creates its own notification. The user sees three cards that are really one concern, and the fact that *three* agents converged on it — the strongest priority signal we have — is buried as noise instead of surfaced as importance.

Boss already has the notification substrate. The agent-authored, human-actionable notification feature (design: [`attentions`](attentions.md)) is the **AttentionGroup** + **Attention** pair surfaced in the macOS "Notifications" toolbar window. An `AttentionGroup` is one card; its `Attention` members are the questions / followups inside it. A single card can bundle *several distinct* `Attention` items, only some of which may overlap with items in a card from a different task. (This is distinct from `WorkAttentionItem`, the legacy *engine-raised operational* alert store tied to executions — that store is **out of scope** here; see Non-goals.)

That substrate already dedupes **exact** matches at two granularities:

- **Group-level**, via `grouping_key` + `generation`: `resolve_or_create_group` joins a new attention into the latest open / partially-answered group for its grouping key (`engine/core/src/work/attentions.rs`), and a `(grouping_key, generation)` unique index makes the group idempotent.
- **Member-level**, via `content_key`: `reconcile_attentions` skips members whose `(kind, question_type, prompt_text, source_anchor, proposed_name)` tuple already exists in the group (`engine/core/src/work/attentions.rs`), so re-running the same source is a no-op.

What it does **not** catch is the case this project targets: two agents flag *the same concern* but compute *different grouping keys* (different source runs, different phrasings, different anchors). Exact-match dedup can't see that "The migration in `schema_init.rs` is missing an index on `created_at`" (an `Attention` item inside task A's card) and "Add an index for the attention-items query — full scans on startup" (an `Attention` item inside task B's card) are the same thing. Today those become two separate `Attention` items in separate cards. We want an LLM to recognize the near-duplicate at the **item** level, fold it into one **canonical** `Attention`, **increment a score** on that canonical item each time it happens, and surface the score as a priority affordance — all behind a feature flag, off by default.

A second distinct failure mode motivates a scope extension: attentions are sometimes created that duplicate **existing scheduled work items** — tasks, chores, revisions, or projects already tracked and actioned in Boss. The human is then forced to track the same concern in two places (Notifications window + work-item view), creating confusion and cognitive overhead. The dedup decision must therefore compare a candidate attention against **both** existing open attentions *and* existing work items in relevant non-terminal states. "This attention is already covered by scheduled row T<n>" is a suppress/link verdict on equal footing with attention-vs-attention dup.

Beyond strict duplication, a **sensibility filter** may also winnow attentions that are stale, self-evidently moot, or not actionable. The bar here is deliberately conservative — a false suppression of a real attention is worse than letting a near-dup through. Any suppression must record provenance so suppressed items are auditable, never silently dropped.

### What is already built vs. what this adds

| Already implemented | This project adds |
|---|---|
| `AttentionGroup` / `Attention` data model + `attention_groups` / `attentions` tables | A `score` column on `attentions` + an `attention_merges` provenance ledger |
| Exact group dedup (`grouping_key` + `generation` unique index) | A semantic **near-duplicate** decision (LLM) layered *on top of* exact member dedup |
| Exact member dedup (`content_key` in `reconcile_attentions`) | Creation-time redirect: fold a near-dup candidate `Attention` into the canonical `Attention` instead of creating a new item |
| Anthropic API substrate (`pane_summary::claude_short_summary`, `live_status::SummarizerOutcome`) | A structured-output **dedup decision** call reusing that substrate |
| `merge_poller` background-sweep pattern (`run_one_pass`) | A bounded, idempotent **startup sweep** over existing `Attention` items |
| `AttentionsView` Notifications window | A score/priority affordance + score-aware ordering |
| `boss-feature-flags` framework (`FeatureFlagsStore`, debug-pane toggle) | A `notification_dedup` flag registered in the `REGISTRY` with `default_enabled: false`, togglable live from Debug → Feature Flags |
| Work-item DB (tasks, chores, revisions, projects) | **Taxonomy-aware verdict**: compare candidate against open work items in non-terminal states; suppress/link when "already covered by T\<n>" |
| (none) | **Sensibility filter**: LLM judgment that an attention is stale/moot/not-actionable; suppress only on High confidence with auditable provenance |

## Naming

- **Notification / concern** — in this doc, an individual `Attention` (item) inside a card. The same logical concern can exist as near-duplicate `Attention` items inside different cards produced by different tasks. The `AttentionGroup` (card) is the *container* — not the unit of dedup.
- **Candidate** — an `Attention` about to be persisted (creation-time path) or an existing `Attention` being re-examined (sweep path).
- **Canonical** — the surviving `Attention` a duplicate is folded into. Its score is incremented; it may receive bounded minor edits.
- **Dedup decision** — the LLM transform: `(candidate Attention, comparison set of Attentions) -> is-duplicate? which canonical Attention? proposed minor edits?`.
- **Score** — an integer on each `Attention`: the number of independent reports folded into it. A fresh `Attention` has score `1`. Each fold increments it.
- **Fold** — the act of reconciling a duplicate into a canonical: `score += 1`, write an `attention_merges` row, optionally apply bounded edits, and (sweep path) retire the loser item. If folding empties a card's open items, retire the card too.
- **`attention_merges`** — the durable provenance ledger; one row per fold. Records which `Attention` was folded into which canonical `Attention`, the model, the decision rationale, and any edits applied. Also the sweep's idempotency key.
- **Work-item dup** — a candidate or existing `Attention` that is already covered by a scheduled work item (task, chore, revision, project) in a non-terminal state. Verdict: suppress-with-pointer at High confidence; link (create with a `linked_work_item_id`) at Medium confidence.
- **Sensibility filter** — the LLM judgment that an `Attention` is stale, moot, or not currently actionable, independent of whether an explicit dup target exists. Gated by a separate sub-flag; only suppresses on High confidence with a stated reason.
- **Suppress-with-pointer** — a verdict where the candidate `Attention` is never persisted (creation path) or retired in the sweep, with a durable provenance row that records the covering work-item id or sensibility reason. Never a silent drop.
- **`WorkItemBrief`** — the compact rendering of a work item (id, kind, title, one-line description/status) passed to the dedup-decision LLM alongside `AttentionBrief` items to enable taxonomy-aware comparison.

---

## Goals

- Recognize **near-duplicate** `Attention` items that exact-match dedup misses, using an LLM, at two trigger points: **creation-time** (before a new `Attention` is persisted) and a **startup sweep** (a bounded pass to catch dups that slipped through).
- Treat "N agents reported the same thing" as a first-class **priority signal**: a `score` on each `Attention` item, incremented atomically on every fold, surfaced as a priority affordance in the Notifications UI.
- Allow the LLM to fold *new information* from a duplicate into the canonical via **minor, bounded edits**, with full **provenance** (we can always tell an `Attention` was edited by a merge, and which duplicate caused it).
- Gate the entire behavior behind a **feature flag**, **off by default** (off-safe), gating both the creation-time and sweep paths. With the flag off, behavior is byte-for-byte today's exact-match dedup.
- Keep the comparison set **tractable** — bounded candidate sets at creation, bucketed comparisons at sweep, no O(n²) blow-up.
- Make the sweep **idempotent**: safe to run repeatedly, never thrashing or looping notifications.
- **Layer on, don't replace**, the existing exact dedup. The cheap deterministic `grouping_key` / `content_key` paths run first and unchanged; the LLM only adjudicates what exact matching leaves ambiguous.
- Compare candidates against **work items** (tasks, chores, revisions, projects in non-terminal states) as well as open `Attention` items — "already covered by T<n>" is a valid suppress/link verdict so the human ends up with ONE place to track a concern.
- Apply a conservative **sensibility filter** that suppresses attentions that are stale, moot, or not actionable — with a high-confidence threshold, a stated reason, and full audit provenance. Suppressed items are retained and inspectable, never silently dropped.

## Non-goals

- **`WorkAttentionItem` (legacy operational alerts).** Those are engine-raised (worker failed, repo unresolved, tracker sync failed), not agent-authored, and already idempotent via `upsert_external_tracker_attention`. They are not what the user means by "notifications" here and are out of scope. (If we ever want dedup there, the `attention_merges` ledger shape generalizes — noted as future.)
- **Re-clustering / un-merging.** Once folded, a duplicate stays folded. We do not build an "actually these were different, split them" path in v1 (an operator can dismiss the canonical and the source agents will re-flag).
- **Cross-product dedup.** The comparison scope is a single product. An `Attention` in product A never folds into one in product B.
- **Replacing exact dedup with the LLM.** `grouping_key` / `content_key` remain the first and cheapest line; the LLM is strictly additive.
- **Embeddings / a vector index.** v1 uses a cheap lexical prefilter + an LLM adjudication. A learned embedding index is a future optimization, not a v1 blocker.
- **Mini coordinator agent at creation time.** Spawning a full LLM agent with `boss` CLI access for every creation-time dedup verdict is too slow (seconds per invocation) and too expensive for the synchronous creation path. The structured-context approach (DB snapshot + single LLM call) covers the same judgment without the latency; the mini-agent is noted as a potential future escalation path for edge cases that require external API queries. See Alternative E below.
- **Rewriting notification content wholesale.** Canonical-edit-on-merge is deliberately *minor and bounded* (see Chosen approach); it is not a summarization or merge-of-bodies feature.
- **A periodic background sweep on a timer.** The sweep runs on engine startup only (the project's stated "likely just on startup, since it should rarely be needed"); a recurring timer is explicitly deferred.

---

## Alternatives considered

### Alternative A — Pure deterministic dedup (extend `grouping_key` / embeddings, no LLM)

Make the dedup smarter without an LLM: normalize and fuzzy-hash the grouping key, or compute embeddings for each notification and fold when cosine similarity exceeds a threshold.

**Rejected for v1.** A better hash still only catches lexically-close keys — it cannot tell that two differently-phrased concerns are the same, which is exactly the failure mode (different agents, different wording, different anchors). Embeddings get closer but introduce a model + vector store + a similarity threshold to tune, and a threshold-only decision still can't produce the *bounded minor edit* the project requires ("fold in new info"). The semantic judgment ("are these the same concern, and what does the second add?") is precisely what an LLM is good at and a similarity score is not. Embeddings remain attractive as a future *prefilter* (cheaper candidate selection) feeding the same LLM adjudication — captured as a future task, not a v1 path.

### Alternative B — Always create, then sweep-only reconciliation (no creation-time check)

Let every agent create its notification freely; rely solely on a periodic/startup sweep to fold dups afterward.

**Rejected as the sole mechanism.** It guarantees a window where the user sees N duplicate items before the sweep runs, and an on-startup-only sweep means that window can be hours. It also makes the score lag reality. Creation-time dedup keeps the Notifications window clean in the common case (an agent flags something that's already there) and makes the score accurate the moment the second agent reports. The sweep is kept as a **backstop** (Goal: "catch dups that slipped through"), not the primary path. The chosen design does both, with creation-time as primary.

### Alternative C — Spawn a worker to do the reconciliation

Spawn a normal Claude worker whose prompt is "look at the open notifications and merge the duplicates."

**Rejected.** This is the wrong tool for a tight, frequent, structured decision. A worker is heavyweight (a cube lease, a full agent session), slow, costly, and returns prose we'd have to parse. It also can't be invoked synchronously inside the notification-creation transaction. The decision here is a bounded prose-to-JSON transform — exactly what the existing engine-internal Anthropic substrate (`pane_summary.rs`) does for pane summaries — so a direct, structured-output API call is the right shape. (Same reasoning the [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) design used to reject an interactive worker for its Planner.)

### Alternative E — Mini coordinator agent with `boss` CLI access

Spawn a small LLM agent that has access to the `boss` CLI. Given a candidate attention, it queries what work items and attentions already exist (e.g. `boss task list --product P<n>`, `boss attention list`, etc.) and returns a verdict: keep / suppress-as-dup-of-X / merge-into-X. The agent assembles a dynamic picture of current state at decision time.

**Evaluated for taxonomy-aware and sensibility judgments; not chosen as the primary mechanism.** The mini-agent has genuine strengths: it can adapt its queries to the candidate, handle edge cases the prompt can't anticipate, and detect staleness that a static snapshot might miss (e.g. referencing an external PR that was just closed). However, the creation-time path is synchronous — every attention creation blocks on the verdict. An agent that runs CLI sub-commands takes several seconds per invocation at minimum, making the creation path unacceptably slow under any real load. It is also significantly more expensive (multiple LLM calls + tool calls per decision vs. a single cheap-tier call) and non-deterministic (harder to test, harder to reason about failure modes). The dynamic-query capability is also largely unnecessary for the common cases: Boss stores all work items in a shared SQLite DB that the engine can query directly; the "dynamically assembled state" the agent builds via CLI is a slower, costlier read of the same data.

**Retained as a future escalation path** for complex sensibility edge cases where the static DB snapshot genuinely cannot answer the question — for example, an attention that references an external repository's PR state, where staleness detection requires an API call outside the local DB. For v1, the structured-context approach (Alternative D) is chosen for all paths.

### Alternative D (chosen) — Engine-internal LLM dedup decision at the Attention (item) level, layered on exact dedup, at creation + startup sweep

Keep exact dedup as the first line. When (and only when) a new `Attention` would be persisted and exact matching does not apply, run an engine-internal structured-output LLM call against a bounded set of (a) existing open `Attention` items and (b) work items in non-terminal states, all within the same product; if it returns a canonical attention match, fold instead of create (`score += 1`, provenance row, bounded edit, empty-card cleanup); if it returns a work-item match, suppress-with-pointer or link per the confidence tier. The same call also applies the sensibility filter (is this attention stale/moot?). A bounded, idempotent startup sweep applies the same decision to existing `Attention` items as a backstop. Everything behind off-by-default flags (one parent flag + sub-flags for taxonomy and sensibility). This is the rest of the document.

---

## Chosen approach

### Architecture overview

```
  CREATION-TIME PATH                                     STARTUP SWEEP PATH
  create_attention / reconcile_attentions                engine boot, flag on
        │                                                       │
        ▼                                                       ▼
  exact dedup first  ─── grouping_key / content_key       bucket open Attentions by
  (unchanged)             match? → join existing,          (product[, kind])
        │                 no LLM, return                         │
        │ would create a NEW Attention (item)                    ▼  per bucket, bounded
        ▼  (flag on?)                                       pick canonical (oldest/lowest A#
  prefilter open Attentions (same product, recency         item in the product)
  window) → top-K candidate set (cross-card, cross-task)  compare each other vs canonical
        │                                                            │
        ▼                                                       ▼
  LLM dedup decision (candidate Attention vs top-K)      LLM dedup decision (batched)
        │                                                       │
   duplicate? ── no ──► persist Attention (score=1)       fold losers: score += n,
        │ yes                                              attention_merges rows,
        ▼                                                  retire loser item
   FOLD into canonical Attention:                          (merged_into_attention_id +
     • score += 1  (atomic)                               optional bounded edit)
     • attention_merges provenance row                         │
     • bounded minor edit to canonical (optional)             ▼
     • suppress candidate (do not persist new item)      empty-card cleanup: if a group's
        │                                                last open item was folded,
        ▼                                                retire the group (state=dismissed)
   return canonical → AttentionCreated event → UI               │
                                                                ▼
                                                         idempotent: already-merged &
                                                         already-compared pairs skipped
```

The engine owns everything. The single LLM step is the dedup decision; it is a pure transform (no writes). Exact dedup runs first and unchanged. The flag gates both paths.

### 1. The unit of dedup is the individual `Attention` (item); the scope spans tasks within a product

A single `AttentionGroup` (card) can bundle several distinct `Attention` items, only some of which may overlap with items produced by a different task. The card is therefore the wrong granularity — the same actionable concern can appear as `Attention` items inside different cards from different tasks. So:

- **Dedup, score, and provenance are item-level.** The candidate is a would-be or existing `Attention`; the canonical is an existing `Attention` (which may live in a *different* `AttentionGroup` than the duplicate). One card ≠ one notification.
- **The comparison scope is all open `Attention` items within a product — NOT partitioned by card, originating task, or association.** The primary target case is *different tasks* raising the same concern as different `Attention` items: task A and task B each produce an item "missing index on `created_at`" with different phrasings, different anchors, living in different cards. Partitioning the comparison set by association (task/project) or by card makes this case structurally invisible and defeats the feature's purpose. Cross-product dedup remains a non-goal; the correction is purely intra-product, cross-task, cross-card.
- **The LLM layers on top of the member-level exact `content_key` dedup**, not (only) the group-level `grouping_key` path. The `content_key` path still handles exact item matches with no LLM; the LLM only runs for near-duplicate items that exact matching misses.
- **The LLM reasons over rendered item content**, not raw keys: for each `Attention` we build a compact text rendering (kind, association, `prompt_text` / `proposed_name` + `proposed_description` / `rationale`). The `association` field appears in the rendering as context for the LLM — it is not used to filter or partition the candidate set.

### 2. Data model — score + provenance

#### `score` on `attentions`

A single new column at the **item level**, added by an idempotent migration in the established style (`engine/core/src/work/migrations_b.rs`):

```sql
ALTER TABLE attentions ADD COLUMN score INTEGER NOT NULL DEFAULT 1;
```

- **Default `1`** — a freshly created `Attention` has been "reported once." This makes the score a clean count of independent reports and means existing rows backfill to a sensible `1` with no data migration.
- **Atomic increment** — folding always runs inside the existing creation/sweep transaction:
  ```sql
  UPDATE attentions SET score = score + ?delta WHERE id = ?canonical_id;
  ```
  Single-statement, single-transaction; no read-modify-write race. (Creation folds `delta = 1`; the sweep may fold a cluster's loser count in one statement.)
- **Mapper + protocol.** `score: i64` is added to `boss_protocol::Attention` (a struct — additive optional fields need only `#[builder(default = 1)]`) and read in `map_attention` (`engine/core/src/work/mappers.rs`), which must explicitly map the new column (DB mappers stay struct-literal per repo convention).
- **Card-level score affordance.** The `AttentionGroup` protocol type may expose a derived `max_item_score` or `total_score` field computed from its members, for UI rendering — but the source of truth is on `attentions`, not `attention_groups`.

#### `linked_work_item_id` on `attentions` (Medium-confidence work-item dup)

```sql
ALTER TABLE attentions ADD COLUMN linked_work_item_id TEXT;  -- nullable, e.g. "T42"
```

Set when a `WorkItemDup` verdict comes back at Medium confidence at creation time or during the sweep: the attention is created/retained normally but stamped with the covering work item's id. The UI renders a "see T<n>" cross-reference chip. This is the single field that gives the human a one-click path from a still-visible attention to the canonical work item. Excluded from the comparison set once set? No — a linked attention remains open and actionable; the human dismisses it when they're satisfied the work item covers it.

#### `merged_into_attention_id` on `attentions` (sweep retirement)

```sql
ALTER TABLE attentions ADD COLUMN merged_into_attention_id TEXT;  -- nullable, references attentions(id)
```

The **creation-time** path *suppresses* the candidate before it is persisted — there is no loser row to retire. The **sweep** path, however, operates on already-persisted items; a folded loser is retired by setting `merged_into_attention_id = <canonical_attention_id>` and `answer_state = 'merged'` (a new terminal member state). This (a) removes it from the actionable list, (b) preserves it for history/provenance instead of deleting, and (c) is the marker that makes the sweep idempotent (a member with `merged_into_attention_id` set is never re-considered).

#### Empty-card cleanup (sweep path)

After retiring a loser `Attention`, check whether the card (`AttentionGroup`) that contained it has any remaining open members (members with `answer_state` not in `{'answered', 'skipped', 'merged'}`). If **all members are now non-open**, retire the card: `state = 'dismissed'`, `dismissed_at = now()`. Do not leave empty cards in the Notifications window. This check runs inside the same fold transaction; it is a single indexed query (`SELECT COUNT(*) FROM attentions WHERE group_id = ? AND answer_state NOT IN (...)`) and is cheap.

#### `attention_merges` provenance ledger

One row per fold — the durable record that a fold happened, why, and what changed. Operates at the **`Attention` (item) level**:

```sql
CREATE TABLE IF NOT EXISTS attention_merges (
  id                      TEXT PRIMARY KEY,        -- merge_<...>
  -- Exactly one of canonical_attention_id / canonical_work_item_id is set per row.
  canonical_attention_id  TEXT REFERENCES attentions(id),  -- set for AttentionDup
  canonical_work_item_id  TEXT,                    -- set for WorkItemDup / sensibility (T<n>, C<n>, etc.)
  product_id              TEXT NOT NULL,
  trigger                 TEXT NOT NULL,           -- 'creation' | 'sweep' | 'sensibility'
  -- Creation-time: the candidate is never persisted, so we capture its identity inline.
  -- Sweep: the loser is a real row; its id is recorded and it is retired (merged_into_attention_id).
  duplicate_attention_id  TEXT,                    -- set for 'sweep'; NULL for 'creation'
  candidate_summary       TEXT NOT NULL,           -- the rendered candidate Attention text (what was folded)
  candidate_source        TEXT,                    -- source_run_id / source_task_id / source_kind of the dup
  model                   TEXT NOT NULL,           -- model slug used for the decision
  decision_rationale      TEXT,                    -- the LLM's short "why" note (verbatim)
  edits_applied           TEXT,                    -- JSON: per-field before/after, or NULL if none
  created_at              TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attention_merges_canonical_idx
  ON attention_merges(canonical_attention_id, created_at)
  WHERE canonical_attention_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS attention_merges_work_item_idx
  ON attention_merges(canonical_work_item_id, created_at)
  WHERE canonical_work_item_id IS NOT NULL;
-- Sweep idempotency: never fold the same (canonical item, duplicate item) pair twice.
CREATE UNIQUE INDEX IF NOT EXISTS attention_merges_pair_uq
  ON attention_merges(canonical_attention_id, duplicate_attention_id)
  WHERE duplicate_attention_id IS NOT NULL;
```

`edits_applied` is what answers "was this `Attention` edited by a merge, and by which duplicate?" — it stores the canonical's affected fields before and after, keyed to the `attention_merges.id`, so the UI can show "edited by merge" provenance and an operator can audit every change.

### 3. The dedup decision (the one LLM step)

A pure transform reusing the existing engine-internal Anthropic substrate (`pane_summary::claude_short_summary` pattern: shared reqwest client, `x-api-key` + `anthropic-version` headers, POST to `api.anthropic.com/v1/messages`; typed outcomes modeled on `live_status::SummarizerOutcome` — `Success` / `NoApiKey` / `ApiError` / `Transport`). It performs no writes.

#### Contract

```rust
// boss-protocol — shared so every caller and tests speak the same shape.

pub struct DedupInput {
    pub candidate: AttentionBrief,        // rendered candidate Attention (kind, assoc, item text)
    pub existing_attentions: Vec<AttentionBrief>,   // top-K open Attention items (same product)
    pub existing_work_items: Vec<WorkItemBrief>,    // non-terminal work items (same product)
    pub sensibility_check: bool,          // whether to also evaluate keep/suppress-sensibility
}

pub struct WorkItemBrief {
    pub work_item_id: String,             // "T<n>" | "C<n>" | "R<n>" | "P<n>"
    pub kind: String,                     // "task" | "chore" | "revision" | "project"
    pub title: String,
    pub status: String,                   // non-terminal state string for context
    pub description_snippet: String,      // first ~200 chars
}

pub struct AttentionBrief {
    pub attention_id: String,             // canonical item id, or "candidate" sentinel
    pub group_id: String,                 // which AttentionGroup this item belongs to (context only)
    pub kind: String,                     // "question" | "followup"
    pub association: String,              // project/task label, for scoping context
    pub rendered: String,                 // item text rendered to prose
}

pub struct DedupDecision {
    pub verdict: DedupVerdict,
    pub confidence: Confidence,            // High | Medium | Low
    pub rationale: String,                 // short "why" — persisted to attention_merges
    pub proposed_edits: Vec<CanonicalEdit>, // bounded; may be empty; only valid for AttentionDup
}

pub enum DedupVerdict {
    Keep,                                   // not a dup, not stale — persist normally
    AttentionDup { canonical_attention_id: String }, // fold into canonical Attention
    WorkItemDup  { work_item_id: String },  // covered by a scheduled work item
    Sensibility  { reason: String },        // stale/moot/not-actionable; High confidence only
}

pub struct CanonicalEdit {
    pub field: EditableField,             // which field of the canonical Attention to edit
    pub new_text: String,                 // the appended/replacing text (bounded length)
}

pub enum EditableField { RationaleAppend, DescriptionAppend }
pub enum Confidence { High, Medium, Low }
```

- **Structured output is enforced**, not requested: a single forced tool call whose input schema is `DedupDecision` (the same forced-tool pattern the codebase will need; `pane_summary` currently uses plain text, so this is the one substrate extension). The engine deserializes straight into the Rust type; a deserialization failure is a decision failure (fail safe → treat as `Keep`, create normally), never parse-and-hope.
- **`AttentionDup` requires a `canonical_attention_id` that is in `existing_attentions`.** `WorkItemDup` requires a `work_item_id` that is in `existing_work_items`. A hallucinated id for either variant → treated as `Keep` (fail safe). Validated in engine code, not trusted from the model.
- **`Sensibility` verdict is only acted on at `High` confidence.** `Medium` or `Low` sensibility → treat as `Keep`. The asymmetry (dup folds on High/Medium, sensibility suppresses only on High) reflects the higher cost of a false-positive sensibility suppression.
- **Only the candidate folds into an existing canonical item** — the decision never proposes creating, deleting, or merging two *existing* items at creation time (that is the sweep's job, and even there only loser→canonical).

#### Model / effort tier

The creation-time decision is **frequent** (potentially every `Attention` being persisted) and **bounded** (one candidate item vs top-K short renderings) — a binary semantic-similarity judgment, not generation. So it defaults to a **fast, cheap tier (Haiku)** with a tight `max_tokens`, mirroring how `live_status` uses a cheap model for its one-liner. Quality is protected by (a) the cheap exact-dedup first line removing the easy cases, (b) the bounded prefilter giving the model only plausible candidates, and (c) confidence handling (below). The model is a single tunable constant. **Open question (R1):** whether Haiku is sufficient or the decision warrants Sonnet — start cheap, measure false-fold rate, upshift if needed. The sweep, being rare and batchable, may use a stronger tier without a frequency concern.

#### Confidence handling

- **High / Medium** → fold.
- **Low** → **do not fold** at creation time (create the candidate normally; a false fold is worse than a missed one — it hides a distinct concern). The sweep, with a human reviewing the Notifications window afterward, may fold Low only when flagged visibly. **Open question (R2):** exact threshold behavior.

### 4. Keeping the comparison set tractable

The comparison set is now `Attention` items, which are more numerous than cards. The prefilter and bucketing must therefore be derived at the item level.

#### Creation-time (1 candidate item vs top-K)

1. **Exact dedup first** (unchanged). The LLM only runs if a new `Attention` item would be persisted that does not already match via `content_key` — most repeat flags never reach it.
2. **Cheap prefilter** to a bounded **top-K** (e.g. K ≤ 32): restrict to open `Attention` items in the **same product** within a **recency window** (e.g. items created/last-touched in the last N days). **The motivating duplicates are deliberately low-lexical-overlap** (different phrasings, different anchors, different source runs — the reason we use an LLM at all), so a pure lexical prefilter would rank genuine cross-task dups below same-card, same-phrasing items and push them out of the top-K. For the prefilter: (a) same-association or same-kind items may rank marginally higher as a *mild tiebreaker*, never a hard filter; (b) the primary ranking lever going forward is a semantic (embedding) prefilter — already noted as a future optimization; until then, widen K and use lexical overlap only as a secondary signal. **Cross-task, cross-card items must have a real chance of entering the top-K.** Only the top-K renderings go to the LLM.
3. **Skip entirely** if the prefilter set is empty (no LLM call) — the candidate is novel; create it.

This is **O(open-items-in-product)** for the prefilter (a single indexed query + ranking) and **exactly one** LLM call with a bounded input. No pairwise explosion.

#### Startup sweep (bounded, bucketed)

The naive "compare all pairs" is O(n²). Instead:

1. **Bucket** open `Attention` items by `(product_id[, kind])`. **`association` (task/project) and `group_id` (card) are explicitly NOT part of the bucket key** — the primary target case is different tasks raising the same concern as different items, and a per-task or per-card partition makes that case structurally invisible. Cross-product pairs are never compared (an item in product A is not a dup of one in product B — that exclusion is correct and stays). Whether to further sub-bucket by `kind` is a tractability choice; note that the same concern can surface under different kinds, so `kind` may be at most a secondary ranking factor, not a hard partition that could miss cross-kind dups.
2. Within each bucket, **pick a deterministic canonical item** (lowest `short_id` of the parent card, i.e. oldest A-number; ties broken by `created_at` of the item, then `id`).
3. **Compare each non-canonical item in the bucket against the canonical(s)** in one batched LLM call per bucket (candidate-vs-set, reusing the same contract). Apply a recency window (items created/last-touched in the last N days) and cap the bucket size considered per sweep; `log()` any remainder rather than silently dropping it.
4. Fold losers into their canonical (cluster fold: `score += loser_count` on the canonical item, one `attention_merges` row per loser, retire each loser item, run empty-card cleanup for each affected group).

Removing `association` and `group_id` from the bucket key means product-level buckets are larger. Tractability is maintained via: (a) the recency window, (b) per-bucket caps, and (c) the embeddings prefilter as the primary scaling lever once available (see R6). **Do NOT reintroduce a per-task/association or per-card partition to keep cost down** — that is exactly what breaks the feature.

#### Work-item side of the comparison set (taxonomy-aware dedup)

When `notification_dedup_taxonomy` is enabled, `DedupInput.existing_work_items` is populated alongside the attention set:

- **Work-item kinds:** tasks, chores, revisions, projects — all four.
- **Non-terminal states:** any state that is not a terminal/closed state. Concretely: tasks and chores in `open | in_progress | review | paused`; revisions in `in_progress | in_review`; projects in `planned | active`. The query is `WHERE state NOT IN ('done','cancelled','archived','merged','closed')` scoped to the same product.
- **Scope: same product, no recency window.** Unlike attentions (where a recency window prevents the comparison set growing unboundedly), work items in non-terminal states are by definition still active — a task open for three months is still work the human is tracking. Include all non-terminal items in the product. If the combined set (attentions + work items) exceeds K, attentions rank above work items as the primary dup target; work items beyond the cap are `log()`-ed, not silently dropped.
- **K (combined cap):** raise the per-call cap to accommodate both sets (e.g. K ≤ 24 attentions + 24 work items = 48 total). Open question R15 covers the concrete value.
- **The LLM prompt** receives the two lists under distinct headings ("Existing open notifications:" and "Existing scheduled work items:") so it can make the distinction clear in its rationale.

#### Work-item dup verdict handling

| Verdict | Confidence | Action at creation | Action in sweep |
|---|---|---|---|
| `WorkItemDup { work_item_id }` | High | Suppress-with-pointer: do not create the Attention; write an `attention_merges` row with `canonical_work_item_id` set. | Retire the loser Attention: `answer_state='merged'`, `merged_into_attention_id` = NULL, `canonical_work_item_id` set in the merge row. |
| `WorkItemDup { work_item_id }` | Medium | Link: create the Attention normally but set `linked_work_item_id` on the new row; badge it in the UI. | Leave open; update `linked_work_item_id` if not already set. |
| `WorkItemDup { work_item_id }` | Low | Keep: create normally. | Leave open. |

This means the human ends up with ONE place to track a concern at High confidence (the work item), while Medium keeps the attention visible but cross-referenced.

**Score on work-item dup:** The score concept (§2) is defined as a count of independent `Attention` reports folded into a canonical `Attention`. When the dup target is a work item, there is no canonical `Attention` to increment. Score is **not incremented** for `WorkItemDup` verdicts in v1. The `attention_merges` row records `canonical_work_item_id` for provenance; a future enhancement could surface a "N related attentions suppressed" counter on the work item itself, but that is deferred (see R11).

#### Sensibility filter

When `notification_dedup_sensibility` is enabled, the same `decide_dedup` call also evaluates whether the candidate attention is stale, moot, or not actionable (`sensibility_check: true`). This is a combined judgment, not a second LLM call. The model is given the candidate attention and asked: "Is this attention already covered by an existing item? Separately, is this attention itself still actionable — or is it stale (references something that no longer exists or is resolved) or not actionable (too vague to act on)?" The verdict `Sensibility { reason }` is returned only at High confidence; Medium and Low are treated as `Keep`.

**Conservative bar — false suppression is worse than a false positive:**

- The model must give a specific, verifiable reason for suppression (e.g. "references PR #42 which is now merged" or "task T<n> already explicitly covers this"). Vague sensibility reasons (e.g. "this seems low priority") are insufficient and must be rejected in the engine's verdict validation.
- The engine validates that a `Sensibility` reason is non-empty and references a specific, checkable fact, not a subjective judgment. Anything failing this check is treated as `Keep`.
- `Sensibility` suppression is **never applied to already-answered or already-linked attentions** — the filter only affects candidates at creation time, and open items in the sweep.

**Provenance for sensibility suppressions:**

The `attention_merges` table is extended with `trigger = 'sensibility'` (alongside existing `'creation'` and `'sweep'`). The `candidate_summary` and `decision_rationale` fields record what was suppressed and why. Suppressed items are retained (creation path: never persisted — the inline `candidate_summary` in the merge row is the only record; sweep path: `answer_state='merged'`, `merged_into_attention_id` NULL, trigger='sensibility'). An operator can query `SELECT * FROM attention_merges WHERE trigger = 'sensibility'` to audit every sensibility suppression.

### 5. Canonical-edit-on-merge — bounded + recorded

The LLM **may** fold new information from the duplicate into the canonical, but tightly constrained. The canonical is an individual `Attention` item:

- **Append-only to free-text fields only.** Editable fields are `Attention.rationale` and `Attention.proposed_description` (followup items) — the *explanatory* prose. **Never** editable: `question_type`, `choice_options`, `kind`, `prompt_text` of a question (changing the question itself would invalidate an in-progress human answer), `answer`, `answer_state`, `association`, or the item's membership in its group.
- **Length-bounded.** Each `CanonicalEdit.new_text` is capped (e.g. ≤ 200 chars), and the total edit per fold is capped. Over-budget edits are rejected (the fold still happens; the edit is dropped and `log()`-ed).
- **Only on still-open items.** Edits apply only to items whose `answer_state == "open"`. An item a human has already answered/skipped is frozen — a merge must never rewrite content under a human's feet.
- **Recorded verbatim.** Every applied edit is stored in `attention_merges.edits_applied` as `{field, before, after}`. This is the provenance: the UI can render an "edited by merge" marker, and any change is fully auditable. An empty/no-edit fold (just `score += 1`) is the common, safe default.

### 6. Feature flag

Boss has a feature-flag framework (`boss-feature-flags` crate, `tools/boss/engine/feature-flags/src/lib.rs`) that surfaces flags in the macOS app under **Debug → Feature Flags** and lets an operator toggle them live without rebuilding the engine. Adding a flag is two edits: append a `FeatureFlagSpec` to `REGISTRY`, then call `feature_flags.is_enabled("name")` at the consumer site. The `FeatureFlagsStore` is already threaded through `ServerState` and `Runner` as `Arc<FeatureFlagsStore>`, so new consumers can reach it without new plumbing.

This flag fits the framework's "manual opt-in" pattern (the README's `default_enabled: false` case) — it gates a new risk-bearing LLM path that operators should be able to enable deliberately and kill instantly if it misbehaves.

Three flags are registered. All default `false` (off-safe). The sub-flags are only meaningful when the parent flag is on, but they are independent entries in the registry (no hierarchy in the framework; the consumer code checks `parent AND sub`).

- **Registry entries** (`tools/boss/engine/feature-flags/src/lib.rs`):
  ```rust
  // Parent flag — gates the entire dedup LLM path (attention-vs-attention + score + sweep)
  FeatureFlagSpec {
      name: "notification_dedup",
      description: "Run LLM near-duplicate detection when persisting an Attention item \
                    (creation path) and on startup (sweep). Off by default — enable to \
                    opt in; set to false to kill immediately if it misbehaves.",
      category: "notifications",
      default_enabled: false,
  }
  // Sub-flag — taxonomy-aware comparison against scheduled work items
  FeatureFlagSpec {
      name: "notification_dedup_taxonomy",
      description: "Include non-terminal work items (tasks, chores, revisions, projects) \
                    in the dedup comparison set. Requires notification_dedup. Off by default.",
      category: "notifications",
      default_enabled: false,
  }
  // Sub-flag — sensibility filter (stale / moot / not-actionable suppression)
  FeatureFlagSpec {
      name: "notification_dedup_sensibility",
      description: "Also evaluate whether a candidate Attention is stale or not actionable, \
                    and suppress on High confidence. Requires notification_dedup. Off by default.",
      category: "notifications",
      default_enabled: false,
  }
  ```
- **Default: `false` for all three (off-safe).** With all flags off: exact dedup runs as today, no LLM calls, no sweep. The score column and `attention_merges` table exist in the DB and are forward-compatible.
- **Consumer pattern:**
  - `is_enabled("notification_dedup")` — two sites: creation path (after exact dedup, before prefilter) and boot-time sweep scheduler.
  - `is_enabled("notification_dedup") && is_enabled("notification_dedup_taxonomy")` — one site: the prefilter that populates `existing_work_items`.
  - `is_enabled("notification_dedup") && is_enabled("notification_dedup_sensibility")` — one site: the `sensibility_check: true` flag in `DedupInput`.
  All three read from the `Arc<FeatureFlagsStore>` already available via `ServerState::feature_flags` / `Runner::feature_flags`.
- **Rationale for sub-flags:** attention-vs-attention dedup, taxonomy-aware dedup, and the sensibility filter have different risk profiles and should be independently rollable. An operator might confidently enable `notification_dedup` while holding back `notification_dedup_sensibility` until the suppression logic has been validated on real data. Sub-flags allow surgical rollout without a code change.
- **Degradation independent of the flags:** even with all flags on, `NoApiKey` / `ApiError` / timeout fails safe to "create normally" (creation) or "skip bucket" (sweep) — no flag configuration can block notification creation.
- **Live toggle from the debug pane:** all three are in the registry, so Debug → Feature Flags can flip any of them with immediate effect. No rebuild or restart required. The parent flag is the master kill switch.

### 7. Startup sweep — trigger & idempotency

- **Trigger:** a one-shot background task spawned at engine boot (the `merge_poller`-style pattern — spawned by the coordinator, runs a single `run_one_pass`-equivalent), gated by `notification_dedup_enabled`. Not a recurring timer (deferred; see Non-goals).
- **Idempotency — the cardinal requirement (must not thrash):**
  - **Deterministic canonical selection** (lowest parent `short_id`, then earliest item `created_at`) means repeated runs converge on the same canonical item, never oscillating.
  - **Retired items are inert:** an `Attention` with `merged_into_attention_id` set (or `answer_state = 'merged'`) is excluded from every future bucket, so it can never be re-folded or counted again.
  - **The `attention_merges` pair-unique index** (`(canonical_attention_id, duplicate_attention_id)`) makes a repeat fold of the same pair a no-op at the DB level — a hard backstop against double-counting the score.
  - **No re-splitting, ever** (Non-goal), so the sweep only ever *reduces* the open set; it has a fixed point and reaches it.
  - **Score is never recomputed from scratch** — it is only ever incremented at the moment of a (newly-recorded) fold, so a re-run that finds nothing new changes nothing.
- **Bounded work:** per-bucket caps + `log()` of any remainder (no silent truncation), one batched call per non-trivial bucket.

### 8. Surfacing the score as priority (UI)

- **Protocol/event:** `Attention.score` rides the existing `AttentionCreated` / group-list events; a fold publishes an update so the canonical item's score refreshes live. The card may expose a derived aggregate (e.g. `max_item_score`) for ordering.
- **macOS app (`AttentionsView.swift`, `ChatViewModel.swift`):**
  - A **score badge** on items (and their parent card) when `score > 1` (e.g. a "×3" / "3 agents flagged this" chip), styled as a priority cue.
  - **Score-aware ordering** of open groups: the current open list (newest-first) becomes **max-item-score-desc, then created-at-desc**, so cards containing the most-corroborated items rise to the top. (A small, contained change to the `openGroups` computed property.)
  - **Merge provenance affordance:** where an item was edited by a merge, a subtle "edited by merge" marker; the item's detail can surface the `attention_merges` rationale ("folded 2 duplicate reports").
  - **Work-item cross-reference chip:** when an attention has `linked_work_item_id` set (Medium-confidence work-item dup), show a "see T<n>" chip linking to the covering work item. Tapping/clicking navigates to that work item.
- No new window or RPC surface beyond the score field, the `linked_work_item_id` field, and a read of `attention_merges` for the provenance detail.

### 9. Edge cases

| Case | Handling |
|---|---|
| Flag off | Creation LLM never runs; sweep not scheduled; exact dedup as today. Score column present on `attentions`, defaults to `1`. |
| No API key / API error / timeout (flag on) | Fail safe: create normally (creation) / skip bucket (sweep). Never blocks notification creation. |
| LLM returns a `canonical_attention_id` not in the input set | Treated as not-a-duplicate; create normally. Validated in engine code. |
| Low confidence | Creation: do not fold (create normally). Sweep: fold only if visibly flagged. |
| Candidate shares a `content_key` with an existing item | Exact path (`reconcile_attentions` / `content_key`) handles it; LLM not invoked. |
| Canonical item already answered/terminal | Not an edit target (edits only on `open` items); terminal items are excluded from the comparison set. |
| Sweep run twice | No-op for already-folded pairs (`attention_merges` pair-unique index); deterministic canonical; score unchanged. |
| Cluster of 3+ dup items in the sweep | All non-canonical fold into the single deterministic canonical item; `score += loser_count` in one statement; one provenance row per loser. |
| Edit exceeds length budget | Fold still happens (`score += 1`); the over-budget edit is dropped and `log()`-ed. |
| Empty prefilter set at creation | No LLM call; create the candidate (it's novel). |
| Canonical item lives in a different card than the duplicate | Normal: the canonical may be in any open card within the product. The fold retires only the loser *item*; the canonical's card is unaffected. |
| Partial-card folding: only one item in a card is a dup | Fold that item only; the other items in the same card remain open and unchanged. The card itself is not retired. |
| Empty-card cleanup: folding the last open item in a card | After retiring the loser item, check for remaining open members. If none remain, retire the card (`state = 'dismissed'`, `dismissed_at = now()`). Runs inside the fold transaction. |
| `answer_state = 'merged'` (new terminal state) | The loser item is in a terminal state; excluded from actionable lists, from the comparison set, and from card-emptiness checks. |
| `notification_dedup_taxonomy` off | `existing_work_items` is empty; no work-item comparison; `WorkItemDup` verdict impossible. Existing attention dedup proceeds as before. |
| Work item transitions to terminal state after High-confidence suppress | The suppressed attention was never persisted (creation path) — nothing to recover. If the operator wants the concern re-surfaced, they dismiss+reopen the work item or let the source agent re-flag. (Auditable via `attention_merges`.) |
| `WorkItemDup` Medium confidence | Attention is created with `linked_work_item_id` set. UI shows a "see T<n>" cross-reference chip; the human can dismiss the attention if it's truly redundant. |
| LLM returns `WorkItemDup` with a work_item_id not in `existing_work_items` | Hallucinated id — treated as `Keep` (fail safe). Validated in engine code before acting. |
| `Sensibility` at Medium or Low confidence | Treated as `Keep`; candidate created normally. Only High confidence triggers a sensibility suppression. |
| `Sensibility` with a vague or non-specific reason | Engine rejects the verdict (reason fails validation); treated as `Keep`. Prevents model from suppressing on subjective grounds. |
| `notification_dedup_sensibility` off | `sensibility_check: false` in `DedupInput`; model is not asked the sensibility question; `Sensibility` verdict impossible. |

---

## Risks / open questions

**R1 — Model tier for the creation-time decision.** Default proposed: Haiku (frequent, bounded, binary judgment). *Open:* is Haiku's precision adequate, or does the false-fold cost justify Sonnet? Start cheap; the model is a single tunable constant; measure false-fold rate before upshifting.

**R2 — False folds hide distinct concerns.** Folding two *different* concerns is worse than missing a dup — it silently suppresses a real notification. Mitigations: exact dedup first, bounded prefilter, fold only on High/Medium confidence at creation, full `attention_merges` provenance so a wrong fold is auditable and visible. *Open:* should creation-time folds be *staged* (score++ but the candidate also surfaced with a "possible duplicate" hint) rather than fully suppressed, for the first rollout? Current proposal: suppress on High/Medium, since the sweep + provenance give recourse.

**R3 — Score semantics.** Proposed: `score` = count of independent reports, default `1`. *Open:* should an explicitly-dismissed-then-re-flagged concern increment the *new* generation's score or carry the old one forward? Current proposal: a new generation starts fresh at `1` (consistent with the existing generation reset on terminal groups).

**R4 — Off-safe default vs. discoverability.** Flag defaults off, which means the feature does nothing until an operator opts in. *Open:* is off-by-default the right launch posture, or should it ship on for a canary product first? Proposed: off, flip on after observing sweep behavior on real data.

**R5 — Editing under a human's feet.** Bounded to append-only, open-items-only, length-capped, recorded. *Open:* is even append-to-rationale too much for v1 — should canonical-edit-on-merge be deferred entirely and v1 ship score-only folds? (See task breakdown: the edit task is separable and could be `future`.)

**R6 — Sweep cost on large products (wider scope, item-level buckets).** With `association` and `group_id` removed from the bucket key, product-level buckets contain *all open items* across all tasks and cards in the product — more numerous than card-level buckets would be. Mitigations: (a) a recency window (items created/updated within the last N days) keeps buckets bounded even for active products; (b) per-bucket cap + `log()` of remainder prevents unbounded LLM calls per sweep; (c) the embeddings prefilter (future optimization, already captured as a non-v1-blocker) is the primary scaling lever — once available, it narrows the comparison set to semantically-near items before the LLM call, making O(bucket) tractable without reintroducing a task or card partition. **Do NOT reintroduce a per-task/association or per-card partition to keep cost down** — that breaks the feature. *Open:* recency window duration and per-bucket cap value; whether over-cap buckets defer to a follow-up sweep pass rather than being silently dropped.

**R7 — Structured-output substrate extension.** `pane_summary.rs` currently does plain-text completion; this needs a forced-tool-call / JSON-schema-constrained variant. Low risk (well-trodden API feature) but it is net-new substrate code. *Open:* build it as a small reusable `structured_call` helper alongside `claude_short_summary` so the [`auto-populate`](auto-populate-project-tasks-on-design-pr-merge.md) Planner (which needs the same) can share it.

**R8 — Interaction with `attentions`' generation model.** Folding must respect generations: a dup of an `actioned`/`dismissed` item should form a new item, not fold into a closed one. Handled by excluding terminal items from the comparison set; flagged here so a reviewer confirms it matches the intended [`attentions`](attentions.md) lifecycle.

**R9 — `answer_state = 'merged'` as a new terminal state.** Adding a new member state requires updating every `answer_state` check in the codebase (list queries, state-machine guards, UI rendering). The sweep retirement path writes this value; all read sites must treat it as terminal. *Open:* confirm the full set of `answer_state` consumers before implementing.

**R10 — Verdict space for `WorkItemDup`: suppress vs. link threshold.** Proposed: High → suppress-with-pointer (attention never created); Medium → link (attention created, `linked_work_item_id` set). *Open:* is Medium-confidence link the right call, or should Medium also suppress? The risk asymmetry (false suppression worse than missed dup) favors linking over suppressing at Medium, but an operator may prefer cleaner Notifications window even at the cost of occasional false suppresses. Settle before implementing task 5 (dedup-at-creation).

**R11 — Score on work-item dup.** Proposed: score is NOT incremented when the dup target is a work item (no canonical `Attention` to increment). A future `suppressed_attention_count` field on the work item protocol type would track "N related attentions suppressed" as a signal — but deferred. *Open:* confirm that score-not-incremented is acceptable for v1; if not, decide where the count lives.

**R12 — Single flag vs. sub-flags for taxonomy-aware and sensibility behavior.** Proposed: three independent flags (`notification_dedup`, `notification_dedup_taxonomy`, `notification_dedup_sensibility`) allowing incremental rollout of each capability. *Open:* confirm sub-flag approach is acceptable, or simplify to one flag gating all three behaviors together.

**R13 — Sensibility filter conservatism: is reason-validation in engine code sufficient?** The proposed safeguard (engine rejects a `Sensibility` verdict whose reason string fails a "specific, checkable fact" validation) is necessarily heuristic. *Open:* what is the concrete validation rule? Proposals: (a) reason must contain a reference to a specific entity id (T<n>, PR #n, file path); (b) reason length > N chars (prevents "stale"); (c) separate whitelist of acceptable reason templates. This is important to settle before implementing the sensibility filter task.

**R14 — Startup sweep + sensibility filter scope.** The sweep is designed for attention-vs-attention folding. *Open:* should the sweep also apply the sensibility filter to existing open attentions (evaluate whether already-persisted attentions are now stale)? The primary use case for sensibility is at creation time (catching bad attentions before they appear), and sweep-based sensibility would retire items the human may have already seen. Proposed: sensibility filter is creation-path-only in v1; the sweep applies attention-vs-attention folding + work-item dup detection only.

**R15 — Combined comparison-set cap K for taxonomy-aware dedup.** Proposed: K ≤ 48 total (24 attentions + 24 work items), attentions ranked first. *Open:* the right split depends on typical product sizes (how many open tasks vs. open attentions). If products regularly have 50+ open tasks, the work-item side needs its own prefilter (e.g. most-recently-updated N work items) before the combined cap. Settle by measuring real product sizes before implementing task 4a (taxonomy prefilter).

---

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. Tasks at the same depth with no edge between them may run in parallel.

1. **Schema + score field + provenance ledger** (`boss-engine`). Idempotent migration adding `score INTEGER NOT NULL DEFAULT 1`, `merged_into_attention_id TEXT`, and `linked_work_item_id TEXT` to `attentions`; adding `answer_state = 'merged'` as a recognized terminal value; creating the `attention_merges` table + its indexes (item-level ids including the pair-unique sweep-idempotency index, plus the `canonical_work_item_id` index for taxonomy dup provenance queries). Add `score`, `merged_into_attention_id`, `linked_work_item_id` to `map_attention` / list queries; add `score: i64` and `linked_work_item_id: Option<String>` to `boss_protocol::Attention` (with `#[builder(default = 1)]` and `#[builder(default)]` respectively); `WorkDb` accessors for `attention_merges` (insert/list/count-by-work-item); empty-card-cleanup helper (count open members after fold). **Effort:** `medium`. **Depends on:** none.

2. **Feature-flag plumbing** (`boss-feature-flags`, `boss-engine`). Append all three `FeatureFlagSpec` entries (`notification_dedup`, `notification_dedup_taxonomy`, `notification_dedup_sensibility`, all `default_enabled: false`, `category: "notifications"`) to `REGISTRY` in `tools/boss/engine/feature-flags/src/lib.rs`. Add the two `is_enabled("notification_dedup")` checks (creation path in `attentions.rs`, boot-time sweep scheduler in `app.rs`) and the two sub-flag checks (taxonomy prefilter, sensibility flag in `DedupInput`). No behavior change yet — just the gates, all defaulting off, with live debug-pane toggles. **Effort:** `trivial`. **Depends on:** none.

3. **Structured-output dedup-decision substrate + contract** (`boss-protocol`, `boss-engine`). Define `DedupInput` / `DedupDecision` / `DedupVerdict` / `AttentionBrief` / `WorkItemBrief` / `CanonicalEdit` / `Confidence` in `boss-protocol`; add a reusable `structured_call` helper alongside `pane_summary::claude_short_summary` (forced tool call / JSON-schema-constrained output, typed outcomes modeled on `SummarizerOutcome`); implement `decide_dedup(DedupInput) -> Result<DedupDecision>` with the system prompt (covering attention dup, work-item dup, and sensibility judgments in a single call), model-tier constant (default Haiku), `max_tokens` bound, and engine-side validation (canonical attention id ∈ `existing_attentions`; work item id ∈ `existing_work_items`; sensibility reason non-empty and entity-specific; else `Keep`). No callers yet. **Effort:** `large`. **Depends on:** none (but the prefilter/rendering helpers in task 4 consume its types).

4. **Comparison-set prefilter + rendering helpers** (`boss-engine`). The `AttentionBrief` renderer (item → prose, including association and parent group as context) and the creation-time prefilter (same product, recency window, widened top-K with same-association as a mild tiebreaker only — cross-task, cross-card items must enter the top-K). Pure, unit-testable; shared by creation and sweep. **Effort:** `medium`. **Depends on:** 3 (for the `AttentionBrief` type).

4a. **Taxonomy prefilter + `WorkItemBrief` rendering** (`boss-engine`). Query open work items (tasks, chores, revisions, projects) in non-terminal states for the same product; render each as a `WorkItemBrief`; populate `DedupInput.existing_work_items` up to the per-kind cap; `log()` any overflow. Gated by `notification_dedup_taxonomy`. Pure, unit-testable; shared by creation and sweep. **Effort:** `small`. **Depends on:** 3 (for `WorkItemBrief`), 2 (for the sub-flag check).

5. **Dedup-at-creation path** (`boss-engine`). Hook into `create_attention` / `reconcile_attentions` at the "would persist a new `Attention` item" point: when `notification_dedup` on and exact dedup misses, run prefilter (task 4) + taxonomy prefilter if `notification_dedup_taxonomy` on (task 4a) → `decide_dedup`; handle each verdict:
   - `AttentionDup` High/Medium → fold (atomic `score += 1` on canonical item, `attention_merges` row `trigger='creation'`, suppress candidate, return canonical);
   - `WorkItemDup` High → suppress-with-pointer (`attention_merges` row with `canonical_work_item_id`, no Attention row created);
   - `WorkItemDup` Medium → create Attention with `linked_work_item_id` set;
   - `Sensibility` High (if sensibility flag on) → suppress with `attention_merges` row `trigger='sensibility'`;
   - all else → create normally.
   Fail-safe on any LLM error. **Effort:** `large`. **Depends on:** 1, 2, 3, 4, 4a.

6. **Canonical-edit-on-merge (bounded + recorded)** (`boss-engine`). Apply `DedupDecision.proposed_edits` under the bounds (append-only to `rationale` / `proposed_description`, open-items-only, length caps), record before/after in `attention_merges.edits_applied`, drop+`log()` over-budget edits. Consumed by both the creation and sweep folds. *Separable — could ship as `future` (score-only folds in v1) per R5.* **Effort:** `medium`. **Depends on:** 1, 5.

7. **Startup sweep** (`boss-engine`). Boot-time one-shot background task (flag-gated), `merge_poller`-style: bucket open `Attention` items by `(product[, kind])` — **not** by association/task or card, so cross-task dups are visible within each bucket — apply recency window, deterministic canonical (lowest parent `short_id`, then earliest item `created_at`), batched per-bucket `decide_dedup` (with `existing_work_items` populated if taxonomy flag on; `sensibility_check: false` in the sweep per R14), cluster-fold losers (`score += n` on canonical item, retire via `merged_into_attention_id` + `answer_state='merged'`, one `attention_merges` row each, empty-card cleanup for each affected group), per-bucket cap with `log()` remainder. `WorkItemDup` High → retire loser with `canonical_work_item_id` in the merge row. Idempotent via the pair-unique index + retired-item exclusion + deterministic canonical. **Effort:** `large`. **Depends on:** 1, 2, 3, 4, 4a (6 if edits-on-sweep wanted).

8. **UI priority surfacing** (`app-macos`). `score` badge on items and their parent card (`score > 1`), score-desc-then-recency ordering of open groups (using max item score), and a merge-provenance affordance ("edited by merge" marker + folded-count detail reading `attention_merges`). Thin client over the score field + a provenance read. **Effort:** `medium`. **Depends on:** 1 (score in protocol/events); benefits from 5/7 producing real scores but does not block on them.

9. **Tests: end-to-end dedup + idempotency** (`boss-engine`). Fixtures of near-duplicate `Attention` items (different grouping keys, different cards, same concern); assert creation-time fold (score++, suppression, provenance), partial-card folding (other items in same card untouched), sweep clustering + double-run idempotency (no double count), empty-card cleanup (card retired when last open item folded), flag-off no-op, fail-safe-on-no-api-key, edit-bounds enforcement. Also: taxonomy-aware fixtures (attention dup of existing task → suppress-with-pointer at High, linked at Medium), sensibility fixtures (stale attention suppressed with reason, vague reason rejected → Keep), `WorkItemDup` with hallucinated id → Keep, sensibility at Medium → Keep. **Effort:** `large`. **Depends on:** 5, 7 (6 if shipped).

**Parallelism / graph.** Depth 0 (no deps): **1**, **2**, **3** run in parallel. Depth 1: **4** and **4a** (both need 3; 4a also needs 2) run in parallel. Depth 2: **5** (needs 1,2,3,4,4a) and **8** (needs 1) run in parallel. Depth 3: **6** (needs 1,5) and **7** (needs 1,2,3,4,4a) run in parallel. Depth 4: **9** (needs 5,7[,6]).

**Deferred / not a v1 blocker:**
- **Canonical-edit-on-merge (task 6)** — `future` if R5 lands on score-only folds for v1; the fold path (task 5/7) works without it.
- **Embedding-based prefilter** — `future` optimization replacing/augmenting the lexical prefilter (Alternative A); feeds the same `decide_dedup`.
- **Recurring (timer-based) sweep** — `future`; v1 is startup-only per the project scope.
- **Dedup for `WorkAttentionItem` (operational alerts)** — `future`; out of scope for v1, but the `attention_merges` ledger shape generalizes.
- **Un-merge / re-split** — `future`; explicitly a Non-goal for v1.

---

## References

- [`attentions`](attentions.md) — the `AttentionGroup` / `Attention` model, `grouping_key` / `generation` group idempotency, `content_key` member dedup, lifecycle/generation semantics this design layers on.
- [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) — prior art for an engine-internal structured-output LLM step + deterministic apply; shares the structured-call substrate need (R7).
- Code anchors: `AttentionGroup` / `Attention` (`tools/boss/protocol/src/types.rs`); `create_attention` / `reconcile_attentions` / `resolve_or_create_group` / `list_attention_groups` (`tools/boss/engine/core/src/work/attentions.rs`); `map_attention_group` / `map_attention` (`tools/boss/engine/core/src/work/mappers.rs`); schema + migrations (`tools/boss/engine/core/src/work/schema_init.rs`, `tools/boss/engine/core/src/work/migrations_b.rs`); Anthropic substrate (`tools/boss/engine/core/src/pane_summary.rs`, `tools/boss/engine/core/src/live_status.rs`); config/flag pattern (`tools/boss/engine/core/src/config.rs`); background-sweep pattern (`tools/boss/engine/core/src/merge_poller.rs`); UI (`tools/boss/app-macos/Sources/AttentionsView.swift`, `tools/boss/app-macos/Sources/ChatViewModel.swift`).

---

*Parent project: `Notification dedup + scoring`. Design-first; this doc proposes the implementation task graph above for downstream auto-population.*
