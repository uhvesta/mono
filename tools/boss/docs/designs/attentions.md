# Boss: Attentions

## Problem

Agents working on Boss tasks routinely reach a point where they need the human. A design worker writing a doc has genuine open questions ("should this be one table or two?", "yes/no: do we gate extraction behind a flag?"). An implementation worker finishing a chore notices three follow-on pieces of work worth filing. Today none of this reaches the operator as an *actionable* signal. It lives in the transcript, or at best in an "Open questions" section of a design doc that nobody is paged about. The operator finds out by reading, then has to hand-translate "the agent asked X" into a doc edit or a new task.

There *is* an existing attention surface — `work_attention_items` (`attn_…` ids, the `work_attention_items` table) — but it is a fundamentally different thing: engine-raised **operational alerts** ("repo unresolved", "manifest missing", "CI budget exhausted", "parent PR merged mid-revision"). Those are conditions the system surfaces and that auto-resolve when the underlying state clears. They carry no question, no answer, no proposed work, and no grouping. They are not what an agent raises to *ask the human something*.

This doc designs **Attentions**: actionable notifications an agent raises to pull the human into the loop. Every attention always has an action attached — an attention with no possible action is not an attention. The two launch kinds are **Question** (the agent wants the human to answer something, typically feeding back into a design doc) and **Followup** (the agent proposes a piece of work it noticed while completing a task). Attentions surface in a new **Notifications** toolbar window and, for questions about a design doc, **inline in the design-doc viewer**. Critically, related attentions **batch into groups** so that answering ten questions about one doc produces **one** revision, not ten.

## Goals

- A first-class **attention** concept: an agent-authored, human-actionable notification that always carries an action and is associated with a project **or** a task/chore (provenance + a jump target).
- **Two initial kinds, extensible schema**: `question` and `followup`, with room for more kinds without a schema break.
- **Question types** that shape how the human answers: `yes_no`, `multiple_choice`, `prompt` (free text) — represented in schema and rendered as the matching inline control.
- **Batching / grouping**: related attentions collect into a **group** keyed by `(kind, association, source)`; the group is the unit the human acts on, and actioning a group produces a **single** downstream artifact (one revision for a question group; one batch task-create gesture for a followup group) — never N artifacts for N members.
- A **creation pipeline owned by the engine**: structured emission by the agent is the primary path (design-doc question manifests; end-of-task followup blocks), with post-hoc transcript/doc extraction as a backstop. The UI is a thin client.
- A **clear state machine** with defined "take action" semantics per kind and how the action closes the attention.
- A **`boss attention` CLI** mirroring existing conventions (`--json` envelopes, `--no-input`, `--product`, `T`/`P` selectors), covering create / list / show / answer / action / dismiss.
- **Two app surfaces**: a Notifications toolbar item opening an attentions window, and an inline questions surface in the design-doc viewer. Answering in either surface drives the identical downstream effect.
- Reuse of the existing **`boss task create-revision`** path for question groups whose source design doc still has an open PR.

## Non-Goals

- **Folding `work_attention_items` into this store.** Operational alerts stay where they are and keep their own lifecycle. The new Notifications window may *present* both later behind a category filter, but the stores stay separate (see Alternatives). Renaming/migrating the legacy table is explicitly out of scope.
- **Blocking a worker on an answer.** When an agent raises a mid-task question, the task does not halt waiting for a human. The agent proceeds with its best judgement; the answer feeds a *later* revision. Synchronous "agent waits for human" is a future concern.
- **Building the GitHub PR-review-comment triage UI.** That effort (noted in the revision-tasks design) is a separate producer that may *create* attentions later; this design must not preclude it, but does not build it.
- **A general inbox / chat with the agent.** Attentions are discrete, typed, actionable items — not a conversation thread.
- **Auto-applying answers without producing a reviewable artifact.** Answering a question never silently rewrites a merged doc; it spawns a revision/design task whose output is a PR the human reviews and merges.
- **Auto-accepting followups.** A proposed followup is never turned into a task without an explicit human gesture.
- **Cross-product attentions.** An attention belongs to exactly one product, inherited from its association.
- **New friendly-id scheme beyond a per-product `A<n>` for groups.** Individual member attentions are referenced by primary id in the CLI (like `cir_…` ci-remediation ids today); only the actionable *group* earns a short id.

## Alternatives considered

### A. Extend `work_attention_items` with question/answer/followup columns

Reuse the existing `attn_…` table: add `question_type`, `answer`, `choice_options`, `proposed_*`, and a `group_id`, and discriminate via new `kind` values (`question`, `followup`).

**Rejected.** `work_attention_items` is read by the engine's *operational* loop — a `repo_unresolved` row gates dispatch; rows auto-resolve when the condition clears. Mixing human-answered questions into that table risks engine code treating a question as a dispatch blocker (or auto-resolving a question because some unrelated state changed). The shapes barely overlap: operational alerts have no answer payload, no question type, no grouping, no proposed-work payload, and no notion of "actioning produces a revision". The only genuine overlap is the *UI list*, which we get by giving both stores a common list-presentation shape (below) — not by sharing a table. Keeping a clean store also lets the operational table be renamed later without disturbing this feature.

### B. Pure post-hoc extraction as the primary creation path

Don't change agent instructions at all. Run a supervisor model pass over every completed transcript and every design doc to extract questions and followups, and turn whatever it finds into attentions.

**Rejected as the primary path** (kept as a backstop). Extraction is brittle and lossy for exactly the data we most need: it can't reliably tell a yes/no question from a multiple-choice one, can't recover the *choices*, and can't anchor a question to the doc section it's about (which the inline surface needs). It also costs a model call per transcript/doc and can hallucinate questions or work that the agent never actually raised. Structured emission — the agent telling us, in a fenced manifest, exactly `{type, prompt, choices, anchor}` — is precise, cheap (no extra model call), idempotent, and reuses machinery we already have for design-producing tasks (the `DOC_REF:` sentinel + sibling-manifest pattern). We therefore **lead with structured emission and fall back to extraction**, flagging extracted rows as lower-confidence.

### C. One attention = one downstream artifact (no grouping)

Each question answered immediately spawns its own revision; each followup immediately becomes a task on accept.

**Rejected.** This is precisely the failure the brief calls out: a design worker that emits ten questions would spawn ten revision tasks against one doc, each opening or amending a PR, each needing its own review. Grouping is load-bearing, not a nicety. The group — not the member — is the unit that produces an artifact.

### D. Engine edits the doc directly on answer (no revision worker)

When the human answers, the engine itself rewrites the markdown and commits it.

**Rejected.** It loses the agentic reconciliation that makes the answer *land well* (the answer "yes, two tables" needs a worker to actually restructure the schema section, not a string splice). It can't handle the "doc already merged → needs a fresh PR" case, bypasses review, and fights the PR-branch model. Spawning a revision (open PR) or a fresh design task (merged doc) reuses the pipeline we already trust and keeps a human in the merge loop.

## Chosen approach

An **attention** is a single agent-authored, actionable notification. Attentions never stand alone in the UI — they belong to an **attention group**, the unit the human reads and acts on. The engine owns creation, reconciliation, state transitions, and producing downstream artifacts; the macOS app and the CLI are thin clients over engine RPC.

### Data model

Two new tables, mirroring the conventions in `tools/boss/protocol/src/types.rs` and the migration style in `engine/src/work.rs` (idempotent `migrate_*` functions, `ALTER TABLE … ADD COLUMN` with defaults, partial unique indexes for short ids).

**`attention_groups`** — the actionable unit. Id prefix `atg`.

| column | type | notes |
|---|---|---|
| `id` | TEXT PK | `atg_{nanos:x}_{counter:x}` |
| `product_id` | TEXT NOT NULL | FK `products(id)`; inherited from association |
| `short_id` | INTEGER NULL | per-product `A<n>` friendly id; partial-unique index, mirrors tasks/projects |
| `kind` | TEXT NOT NULL | `question` \| `followup` (extensible) |
| `association_project_id` | TEXT NULL | FK `projects(id)` |
| `association_task_id` | TEXT NULL | FK `tasks(id)` |
| `source_kind` | TEXT NOT NULL | `design_doc` \| `task_transcript` \| `manual` |
| `source_task_id` | TEXT NULL | originating design/impl task (the jump-back target) |
| `source_run_id` | TEXT NULL | transcript pointer (`runs.id`); pairs with `runs.transcript_path` |
| `source_doc_path` | TEXT NULL | repo-relative design-doc path (for `design_doc`) |
| `source_doc_repo_remote_url` | TEXT NULL | canonical repo form |
| `source_doc_branch` | TEXT NULL | head branch for in-review viewing |
| `grouping_key` | TEXT NOT NULL | derived stable key (below); upsert dedup target |
| `generation` | INTEGER NOT NULL | bump per source re-run so a new run never merges into a closed group |
| `state` | TEXT NOT NULL | `open` \| `partially_answered` \| `actioned` \| `dismissed` (default `open`) |
| `produced_artifact_kind` | TEXT NULL | `revision` \| `design_task` \| `tasks` (set on action) |
| `produced_artifact_ref` | TEXT NULL | JSON: revision task id / new task ids / PR url |
| `created_at` | TEXT NOT NULL | RFC 3339 / epoch seconds, repo convention |
| `actioned_at` | TEXT NULL | |
| `dismissed_at` | TEXT NULL | |

CHECK: exactly one of `association_project_id` / `association_task_id` is non-null (mirrors the `work_attention_items` XOR CHECK). Unique index on `(grouping_key, generation)` makes reconciliation an upsert.

**`attentions`** — a single member of a group. Id prefix `atn`. (Distinct from the legacy operational `attn_…` ids; the extra `t` is the only visible difference, so implementation should treat the prefix as a hint only and never key logic on it. A later cleanup may rename the operational prefix — out of scope here.)

| column | type | notes |
|---|---|---|
| `id` | TEXT PK | `atn_{nanos:x}_{counter:x}` |
| `group_id` | TEXT NOT NULL | FK `attention_groups(id)` ON DELETE CASCADE |
| `ordinal` | INTEGER NOT NULL | display order within the group |
| `source_anchor` | TEXT NULL | doc section / heading slug (questions) or transcript offset hint; drives inline placement |
| `answer_state` | TEXT NOT NULL | `open` \| `answered` \| `skipped` \| `dismissed` (default `open`) |
| `created_at` | TEXT NOT NULL | |
| `answered_at` | TEXT NULL | |
| **question fields** | | populated when `group.kind = question` |
| `question_type` | TEXT NULL | `yes_no` \| `multiple_choice` \| `prompt` |
| `prompt_text` | TEXT NULL | the question shown to the human |
| `choice_options` | TEXT NULL | JSON array of strings (for `multiple_choice`) |
| `answer` | TEXT NULL | captured answer: `"yes"`/`"no"`, chosen index/value, or free text |
| **followup fields** | | populated when `group.kind = followup` |
| `proposed_name` | TEXT NULL | pre-fills task name |
| `proposed_description` | TEXT NULL | pre-fills task description |
| `proposed_effort` | TEXT NULL | effort hint (`trivial`…`max`) |
| `proposed_work_kind` | TEXT NULL | `task` \| `chore` \| `project` |
| `rationale` | TEXT NULL | why the agent suggested it |
| `confidence_source` | TEXT NOT NULL | `structured` \| `extracted` (provenance / trust flag) |

The Rust `Attention` and `AttentionGroup` structs follow the repo's builder convention — `#[derive(bon::Builder)]` with `#[builder(on(String, into))]` once they cross the 8-field threshold (both do), `Option<T>` fields auto-optional, `#[builder(default = …)]` for `state`/`answer_state`/`generation`. The production DB mapper functions (`map_attention`, `map_attention_group`) use struct literals so a new column is a compile error until mapped, per the repo convention.

### Grouping model and partial-answer semantics

The **grouping key** is the stable string `kind|association|source-discriminator`:

- **Questions from a design doc**: `question|{project_id}|doc:{source_doc_path}`. All questions a worker raises about one doc collapse into one group.
- **Followups from a task transcript**: `followup|{originating_task_id}`. All followups a worker proposes while completing one task collapse into one group.

`generation` separates re-runs: if the same design task runs again (a revision worker re-emits its question manifest), the engine reconciles into the *open* group of the current generation, or — if the prior group is already `actioned`/`dismissed` — bumps `generation` and starts a fresh group. This is what keeps "one group ⇒ one revision" true across iteration.

**Partial answers (multi-sitting).** Members carry their own `answer_state`. A human can answer 3 of 10 questions now and the rest later; each `answer`/`answer_state` is persisted independently and the group sits at `partially_answered`. Nothing downstream happens until the human **actions** the group.

**Actioning a group is a single, terminal gesture.** At action time the engine requires every member to be in a terminal answer-state — `answered`, `skipped`, or `dismissed`. The UI offers a "skip remaining" bulk control so the human isn't forced to touch every row. Actioning then:

1. produces **one** downstream artifact from the `answered` set (skipped/dismissed members contribute nothing),
2. records `produced_artifact_kind` + `produced_artifact_ref` on the group,
3. transitions the group to `actioned` (terminal).

If, later, the source emits *new* questions/followups, they land in a **new** group (next `generation`) — they never reopen a closed one. This is the mechanism that prevents the N-revisions explosion while still letting iteration continue.

### Creation pipeline (engine-owned, structured-first hybrid)

The engine creates attentions; agents and the CLI are producers, never the source of truth for grouping/reconciliation.

**Questions — primary: structured emission from design docs.** We extend the design-doc worker prompt (the `project_design` execution kind) to emit, alongside the doc and its `DOC_REF:` sentinel, a sibling **questions manifest** at `<slug>.attentions.json` (mirroring the `<slug>.tasks.json` manifest pattern from design-producing-tasks). Each entry is `{ question_type, prompt, choices?, anchor }`, where `anchor` is the heading slug the question pertains to. The existing **`DesignDetector`** (which already fires on `DOC_REF:`, reads the PR, and auto-populates the project design-doc pointer) is extended to parse this manifest and upsert an `attention_groups` row (key `question|{project_id}|doc:{path}`) plus its `attentions` members. Idempotent on the unique `(grouping_key, generation)` index.

**Questions — backstop: extraction.** For docs that ship no manifest (human-authored docs, or a worker that forgot), a lightweight extraction pass reads the doc's "Risks / open questions" section and synthesizes `prompt`-type attentions, flagged `confidence_source = extracted`. Lower trust, no choices, coarse anchors. Gated behind a feature flag initially.

**Followups — primary: structured emission from the transcript tail.** We tighten the worker instructions so that, near task completion, the agent emits a structured **followups block** (a `FOLLOWUPS:` sentinel followed by a fenced JSON array, captured the same way the PR-URL sentinel is captured at the end of a run). Each entry: `{ proposed_name, proposed_description, proposed_effort?, proposed_work_kind?, rationale }`. The completion path — which already stamps `runs.transcript_path` and runs detectors over the transcript tail — parses the block and upserts a followup group keyed to the originating task.

**Followups — backstop: supervisor extraction.** Where no structured block is present, a cheap supervisor pass over the transcript tail extracts candidate followups, flagged `extracted`. This covers old transcripts and uncooperative runs.

> Justification, restated: structured emission is robust (exact type/choices/anchor), cheap (no extra model call), and idempotent, and rides infrastructure we already have (sentinels, sibling manifests, transcript-tail detectors). Extraction is flexible (works on anything) but brittle and costly, so it is strictly a graceful-degradation backstop. Both paths write the same rows, so the UI never has to know which path produced an attention beyond the `confidence_source` flag.

A third path, **explicit/manual emission**, lets a worker raise an urgent mid-task question without waiting for transcript post-processing, via `boss attention create` (below). This writes through the same engine RPC and the same reconciliation.

### Engine behaviour and "take action" per kind

All state transitions go through one engine entry point, `ActionAttentionGroup`, so the toolbar window and the inline doc surface produce identical effects.

**Question group → a doc revision.** On action, the engine gathers the `answered` members (question + captured answer) and:

- **Source doc has an open PR** (the design doc is still in review): create a **revision task** via the existing `boss task create-revision` path, parented to the design task that owns the PR. The revision worker is handed the Q&A set and instructed to edit the doc accordingly and push a commit to the existing PR branch — no new PR. `produced_artifact_kind = revision`.
- **Source doc is already merged** (no open PR): the revision gate ("parent PR open and unmerged") would refuse, so instead create a fresh **`design`-kind task** targeting the same project/doc, seeded with the Q&A, which opens a new PR. `produced_artifact_kind = design_task`. (Open question OQ4 below confirms this fork.)

Either way the group flips to `actioned` and records the produced task/PR so the card can link straight to it.

**Followup group → batch task-create.** On action, the human has marked each member `answered` (accept) or `skipped`/`dismissed` (reject). The engine creates the accepted members as new tasks/chores/projects in one transaction via the existing `CreateTask` / `CreateMany` path, pre-filled from the proposed fields, `created_via = "engine_auto"` (or a new `"attention"` value). One human gesture, 0..N tasks, one group closure. `produced_artifact_kind = tasks`, `produced_artifact_ref` = the new ids.

**Dismiss.** A whole group or a single member can be dismissed without producing anything (`state = dismissed` / member `answer_state = dismissed`).

**Live updates.** The engine pushes `AttentionCreated` / `AttentionGroupUpdated` events over the existing frontend socket (the same mechanism `AttentionItemCreated` uses today), so both app surfaces live-update without polling.

### CLI surface

A new top-level noun `boss attention` under the existing clap command tree (`Commands::Attention { command: AttentionCommand }`), matching `task`/`project`/`chore`. All verbs honour the global `--json`, `--no-input`, `--quiet`, `--product` flags and resolve `T`/`P` selectors via the existing `parse_work_item_selector`. JSON output uses the same `print_entity` envelope pattern (`{ "attention_group": … }`, `{ "attention": … }`, `{ "attentions": [...] }`).

```
boss attention list                       # groups for the resolved product
  [--product <slug|id>]
  [--project P12 | --task T34]             # filter by association
  [--kind question|followup]
  [--state open|partially_answered|actioned|dismissed]   # default: open + partially_answered
  [--members]                              # expand to individual attentions
  [--json]

boss attention show <A12|atg_…> [--json]   # group + members

boss attention create --kind question --question-type yes_no|multiple_choice|prompt \
  --prompt "…" [--choice "A" --choice "B" …] \
  (--project P12 | --task T34) \
  [--group <A12|atg_…> | --group-key <key>]   # join an open group; else engine derives
boss attention create --kind followup \
  --name "…" --description "…" [--effort small] [--work-kind task|chore|project] \
  (--project P12 | --task T34)

boss attention answer <atn_…> \
  ( --yes | --no                # yes_no
  | --choice <index|value>      # multiple_choice
  | --answer "…" )              # prompt
boss attention answer <atn_…> --skip            # mark skipped
boss attention dismiss <A12|atg_… | atn_…> [--reason "…"]

boss attention action <A12|atg_…> [--skip-unanswered] [--confirm]
  # finalize: questions -> one revision/design task; followups -> batch create.
  # --json returns { "attention_group": <group>, "produced": { "kind": …, "ref": … } }
```

**Who creates attentions via the CLI?** The dominant path is engine-side (manifests + extraction during completion processing) and does **not** round-trip through the CLI — the engine writes the store directly. `boss attention create` exists for the explicit mid-task emission case and for tooling/tests; it is a thin RPC client like every other verb. Agents are *encouraged* to use the structured manifest/sentinel emission over imperative `create` calls, because the manifest path is reconciled idempotently and survives re-runs, whereas a bare `create` is a one-shot.

New RPC variants mirror the existing `*AttentionItem*` ones in `wire.rs`: `ListAttentionGroups`, `GetAttentionGroup`, `CreateAttention`, `AnswerAttention`, `ActionAttentionGroup`, `DismissAttention`; events `AttentionCreated`, `AttentionGroupUpdated`, `AttentionGroupActioned`.

### App UI

**Notifications toolbar item.** A new `ToolbarItem(placement: .primaryAction)` in `ContentView`'s `.toolbar`, a bell glyph carrying `.badge(openGroupCount)` when `> 0` (the same badge pattern used elsewhere). Clicking opens the **Attentions window** — a new `WindowGroup("Notifications", id: "attentions", for: …)` scene opened via `@Environment(\.openWindow)`, following the markdown-viewer / design-renderer scene pattern. A window is correct for now; a future iteration may demote it to a popover/panel anchored on the toolbar item — flagged, not built.

**Attentions window — grouped cards.** The list is grouped by `attention_groups`. Each **group card** shows: kind chip, association (a jump link to the project/task and, for questions, to the design doc — opens the existing `DesignRendererView` window), a one-line source summary, member count, `confidence_source` badge when any member is `extracted`, and the kind-appropriate bulk action. Expanding shows member rows with inline controls:

- **Question / `yes_no`** → a segmented Yes / No control.
- **Question / `multiple_choice`** → a radio group / picker built from `choice_options`.
- **Question / `prompt`** → a multi-line text field.
- **Followup** → proposed name + description + effort with an Accept / Reject toggle.

The card footer is a single button: **Submit answers** (questions) or **Create selected** (followups), with a "skip remaining" affordance. Pressing it calls `ActionAttentionGroup` and the group leaves the open list, leaving a link to the produced revision/tasks.

**Inline in the design-doc viewer.** `DesignRendererView` gains a **questions surface** — a collapsible right sidebar (or bottom bar) listing the open question group(s) whose `source_doc_path` matches the doc on screen, with the *same* inline answer controls as the window. Because each question carries a `source_anchor` (heading slug), questions render next to the section they concern, reusing the anchored-placement substrate from the comments-in-markdown-viewer design (text-anchor resilience across edits). Answering here calls the identical `AnswerAttention` / `ActionAttentionGroup` RPC, so a doc revision produced inline is indistinguishable from one produced in the Notifications window.

**Grouped everywhere** so a batch of questions is always answered together and yields one revision.

### Schema and wire summary

- New tables `attention_groups` (`atg_…`, per-product `A<n>` short id) and `attentions` (`atn_…`), added via idempotent `migrate_attentions` in `engine/src/work.rs`. XOR CHECK on the group's association; unique `(grouping_key, generation)`; partial-unique short-id index; FK `attentions.group_id → attention_groups.id` ON DELETE CASCADE.
- New protocol structs `AttentionGroup`, `Attention` in `protocol/src/types.rs` (builder pattern, serde `rename_all`/`skip_serializing_if` per repo convention; question-type and state enums serialized lowercase/snake_case like `EffortLevel`).
- New `FrontendRequest` / `FrontendEvent` variants in `wire.rs` for list/get/create/answer/action/dismiss + push events, mirroring the existing `*AttentionItem*` family.
- New CLI noun `boss attention` with the verbs above.
- Engine: extend `DesignDetector` to parse `<slug>.attentions.json`; extend the completion/transcript path to parse the `FOLLOWUPS:` block; add the optional extraction backstops behind a flag; add `ActionAttentionGroup` producing a revision (`create-revision`), a fresh `design` task, or a batch task-create.
- App: Notifications toolbar item + badge, Attentions window scene, and a questions surface in `DesignRendererView`.

## Risks / open questions

These are the points a reviewer should land on before implementation tasks are filed.

1. **OQ1 — "Take action" + state transition (resolved here, confirm).** Question group → one revision (open PR) or one fresh `design` task (merged doc); followup group → batch task-create. The group transitions `open`/`partially_answered → actioned` on the single action gesture, recording the produced artifact. Confirm this is the desired closure model and that `actioned` is terminal (new questions form a new generation rather than reopening).

2. **OQ2 — Grouping key + partial answers (resolved here, confirm).** Key = `kind|association|source-discriminator`; `generation` separates re-runs. Members carry independent `answer_state`; group sits at `partially_answered` until a single terminal action. Confirm the "skip remaining" bulk step is acceptable UX versus requiring every member to be explicitly answered.

3. **OQ3 — Structured vs. extraction (recommended: hybrid, structured-first).** Confirm we are willing to (a) extend the design-doc worker prompt to emit `<slug>.attentions.json` and (b) tighten worker instructions to emit a `FOLLOWUPS:` block, with extraction as a flag-gated backstop. The alternative (extraction-only) is cheaper to ship but materially worse on precision and anchoring.

4. **OQ4 — Answer → concrete doc revision.** Open-PR docs reuse `boss task create-revision` cleanly. The **merged-doc** path proposes a fresh `design`-kind task that opens a new PR. Confirm that fork (vs., e.g., refusing to action a question group once its doc has merged, or routing through a generic doc-edit chore). Also confirm the revision worker is the right actor to translate raw Q&A into prose edits.

5. **OQ5 — Relationship to existing surfaces (resolved here, confirm).** New store, separate from `work_attention_items`; the Notifications window shows agent attentions now and may present operational alerts later behind a category filter via a shared list-presentation shape. Confirm we do **not** want a unified table, and confirm the `atn`/`attn` id-prefix proximity is acceptable (or we should pick a more distinct prefix such as `aq`/`af` or rename the operational one).

6. **Anchor drift.** Inline question placement depends on `source_anchor` matching a doc heading; doc edits can move/rename headings. Mitigation: reuse the comments-in-markdown text-anchor resilience approach and degrade to a flat list when an anchor no longer resolves. Needs validation that anchors survive a revision round-trip.

7. **Extraction hallucination / cost.** The backstop can invent questions/followups the agent never raised. Mitigation: flag clearly as `extracted`, gate behind a feature flag, and consider requiring human confirmation before an extracted followup can be actioned. Open: do we ship the backstop in v1 or defer it?

8. **Short-id namespace.** Proposal gives *groups* a per-product `A<n>` and references *members* by primary id. Confirm `A` doesn't collide with any planned namespace and that members don't also need friendly ids for CLI ergonomics.

9. **Mid-task questions are non-blocking (non-goal, confirm).** An agent that raises a question via `boss attention create` keeps working with its best guess; the answer lands in a later revision. Confirm we don't want a synchronous "agent waits" mode in v1.
