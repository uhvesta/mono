# Design: Transcript viewer

**Status:** Proposed (design)

**Audience:** engineers working on the Boss engine, `boss`/`bossctl` CLI, and the macOS desktop app.

## Summary

Let a human operator open and read the agent chat transcript of *any* execution of a task, in a dedicated window. The operator picks a task, sees the full history of executions run against it (project_design, revision_implementation, ci_remediation, …), selects one, and reads its agent transcript rendered high-fidelity.

The transcript is the Claude Code JSONL session log. The engine converts JSONL → markdown (one converter, shared by two RPC callers), and the app renders that markdown by reusing the existing MarkdownUI rendering component — but composed as a *lazy list of per-event markdown segments* rather than one giant string, so large transcripts stay responsive and individual events (thinking, big tool results) can collapse.

This is an advanced/power-user feature. It is the realization of work related to "future work" in four existing design docs: [worker-live-status](worker-live-status.md) (the live transcript tail; `--format=markdown`, "a dedicated transcript viewer window"), [macos-modernization-audit](macos-modernization-audit.md) (window management — "a window for browsing historical executions and their transcripts"), [work-kanban](work-kanban.md) ("a 'View transcripts' action"), and [markdown-renderer-migration](markdown-renderer-migration.md) (the markdown renderer; pagination for very large documents).

## Goals

- **Read any execution's transcript.** Surface *historical* runs, not just live ones — retries, revisions, remediations all accumulate as executions on a task and must all be reachable.
- **Execution list per task.** Show every execution for the selected task with its kind, status, model, run-id, and start/end timestamps; let the operator pick one.
- **High-fidelity rendering.** Faithfully render every JSONL event type — user/assistant messages, thinking, tool_use, tool_result, system events, hook/pr-link/attachment events — preserving conversation order, timestamps, model, and code blocks.
- **Stay performant on large transcripts.** Transcripts run to hundreds of messages and hundreds of KB+. Opening and scrolling one must not choke the UI.
- **Engine owns conversion + listing; app is a thin client.** The JSONL → markdown conversion and the execution listing live in the engine. The app lists executions and renders returned markdown; it does not parse raw JSONL itself.
- **Reuse, don't fork.** Reuse the existing markdown rendering component rather than standing up a second markdown renderer ([markdown-renderer-migration](markdown-renderer-migration.md)).
- **Degrade gracefully.** Handle in-progress (partial), missing/rotated/GC'd, and zero-execution cases without erroring.

## Non-goals

- **Editing, replaying, or re-running transcripts.** Read-only. No "resume from here", no annotation/commenting (that is design-renderer future work, not this).
- **A new live-tail experience.** The live agent tail already exists ([worker-live-status](worker-live-status.md)); this viewer is execution-centric and history-first. It will *render* an in-progress execution's partial transcript, but it does not replace or restyle the existing 1 Hz tail view.
- **Cross-task / global transcript search or a global transcript browser.** Scope is "the executions of one task." A global browser is possible future work.
- **Changing the JSONL transcript format or how agents emit it.** We consume what Claude Code writes.
- **Exporting transcripts** (PDF/HTML/share). The markdown is already a portable artifact; export is out of scope for v1.
- **Mobile/web frontends.** macOS app only (mirrors the rest of the desktop feature set).

## Background

What exists today (verified against the code):

- **JSONL transcripts.** Claude Code writes one JSON object per line to `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`, where `<cwd-slug>` is the workspace path with `/` and `.` replaced by `-` (`tools/boss/engine/src/agents/spawn.rs`).
- **Parser.** `tools/boss/engine/src/agents/transcript.rs` parses JSONL into a normalized `Vec<TranscriptEvent>` via serde-tagged `RawLine`/`RawBlock` enums. It tolerates IO errors and skips malformed/partial lines (the live last line is often a partial write), so it already handles in-progress files. The normalized model:

  ```rust
  pub struct TranscriptEvent { pub seq: u64, pub kind: TranscriptEventKind, pub timestamp: Option<String>, pub model: Option<String> }
  pub enum TranscriptEventKind {
      UserText(String), AssistantText(String), Thinking(String),
      ToolUse { name: String, input: serde_json::Value },
      ToolResult { output: String, is_error: bool },
      System { subtype: Option<String>, body: String },
  }
  ```

  Note: `pr-link`, `hook_success`/PreToolUse/Stop hooks, `attachments`, `stop_hook_summary`, and `turn_duration` are **not** distinct variants today — they arrive as `System { subtype, body }` (subtype preserved, content stringified) or folded into user-message content. The converter must render these `System` subtypes legibly, and we may add explicit handling (see task breakdown).

- **RPC transport.** JSON-RPC over a unix domain socket (`tools/boss/engine/src/rpc/`). `agents.transcript { run_id }` resolves `run_id → live supervisor entry → session_id → jsonl path → parse`. Because it goes through the *live* supervisor, it returns "unknown run" for finished (or not-yet-registered) runs — the documented agents-list/transcript-tail divergence. `agents.list` is live-only and has no historical rows.
- **Durable execution records.** The `work_executions` table is the source of truth for historical runs. Crucially it stores `session_id` **and** `transcript_path` (the absolute jsonl path snapshotted at spawn), plus `id, task_id, kind, status, model, run_id, started_at, ended_at, created_at`. `tools/boss/engine/src/db/work.rs` already has `list_executions_for_task(task_id) -> Vec<WorkExecution>` (ordered newest-first) and `map_execution`. **Neither is exposed over RPC yet.** The protocol type is `WorkExecution` in `boss-protocol/src/types.rs` with `ExecutionKind` / `ExecutionStatus` enums.
- **CLI.** `bossctl agents transcript <run-id> --format <text|jsonl>` (clap `ValueEnum`, default `text`) calls `agents.transcript` and formats locally. There is **no `markdown` variant yet**; `--format` was explicitly reserved for it.
- **Markdown viewer (app).** `MarkdownDocView` renders `Markdown(md).markdownTheme(.boss)` (the MarkdownUI package) inside a plain `ScrollView` — **eager, whole-document, no pagination/laziness/windowing.** It is driven by `MarkdownDocRef { title, source }`, where `Source` is `.file(path) | .designDoc(projectId) | .engineText(method, params)` — the last is already a *generic RPC-coordinate* source. `EngineClient.fetchMarkdown(for:)` switches on the source. Windows are value-keyed `WindowGroup`s opened via `openWindow(id:value:)` (`BossApp.swift`).
- **A bespoke transcript renderer already exists.** `TranscriptTailView` is **not** markdown — it's a `List` of typed `MessageRowView`s with `DisclosureGroup`s collapsing thinking/tool sections, polling `agents.transcript` ~1 Hz. It is lazy (via `List`) and already high-fidelity.
- **Execution list UI already exists.** `TaskDetailView` already renders `Section("Executions") { ForEach(detail.executions) { ExecutionRow(exec:) } }` bound to `[ExecutionVM]` (id, kind, status, model, runId, startedAt, endedAt). The "View design" action lives both as a `TaskDetailView` button and a `TaskCardView` context-menu item.

## Alternatives considered

### Alternative A — Render one big markdown string in the existing `MarkdownDocView` (the literal operator steer)

Convert the whole transcript to a single markdown document and feed it to the existing `MarkdownDocView` via a new `MarkdownDocRef.Source.transcript(executionId:)` (or the existing `.engineText` coordinate). Smallest possible app change — a handful of lines in `EngineClient.fetchMarkdown` and a new window scene reusing `MarkdownDocView`.

**Why not (as the whole answer):** `MarkdownDocView` hands the *entire* string to one `Markdown` view, and MarkdownUI builds the full AST eagerly. For a 300 KB / many-hundred-message transcript that means a multi-second hitch on open and sluggish scrolling — exactly the performance cliff [markdown-renderer-migration](markdown-renderer-migration.md) flags as unsolved. It also can't collapse verbose thinking blocks or truncate huge tool results: pure-markdown collapsing relies on HTML `<details>`, which the MarkdownUI theme does not render reliably. So this loses on two explicit goals (performance, collapsible thinking/large output). It is, however, a perfectly good *fallback/v0* and informs the chosen approach.

### Alternative B — App parses JSONL and reuses the bespoke `MessageRowView` list (extend transcript-tail to history)

The app already has a lazy, collapsible, high-fidelity transcript renderer (`TranscriptTailView` + `MessageRowView`). Point it at a historical execution and we get great performance and collapsing for free.

**Why not:** Two problems. (1) It pushes rendering *fidelity decisions* — and, in its current shape, raw-event handling — toward the app, fighting the "engine owns conversion; app is a thin client" constraint and the "read transcripts via the engine's transcript path; don't have the app parse raw JSONL" constraint. (2) It is a *second* renderer to maintain alongside the markdown viewer; the brief explicitly says don't fork a second markdown renderer, and a bespoke transcript renderer is morally the same maintenance burden (its own styling, code-block handling, theme drift). We do, however, borrow its best ideas — `List`-based laziness and `DisclosureGroup` collapsing — in the chosen approach. (Note: `TranscriptTailView` still gets *events* from `agents.transcript`, so "engine parses" already holds there; the real divergence we reject is "app turns events into presentation markup.")

### Alternative C — Server-side pagination: engine returns event ranges, app requests pages

Make `executions.transcript` page-shaped (`{ execution_id, offset, limit }` → a slice of rendered events) and have the app fetch pages as the operator scrolls.

**Why not (for v1):** Real complexity — scroll-position bookkeeping, prefetch, jump-to-event across page boundaries, and a stateful RPC — for a payload that is fundamentally bounded (a finished transcript is a few hundred KB of text, trivial to transfer once). Lazy *rendering* on the client (Chosen approach) solves the actual cost (AST construction), and transfer cost is a non-issue. We keep paging in our back pocket for pathological multi-MB transcripts (see Risks) but do not build it now.

## Chosen approach

**Engine converts JSONL → a structured list of markdown *segments*; the app renders them lazily, reusing the MarkdownUI component (not the eager `MarkdownDocView`) with per-segment collapsing.** This is Alternative A's engine-owned-conversion + markdown-reuse, fixed with Alternative B's laziness + collapsing, without forking a renderer or pushing parsing into the app.

### Data flow

```
work_executions row (session_id, transcript_path)
        │  engine: read jsonl → parse_transcript → events_to_markdown_segments
        ▼
executions.transcript RPC ──► [TranscriptSegment{ id, role, label, timestamp, model,
        │                                          markdown, collapsible, default_collapsed,
        │                                          truncated }]  (+ is_live, complete)
        ▼
app: TranscriptView → List/LazyVStack → per segment:
        Markdown(segment.markdown).markdownTheme(.boss)   ← same renderer, one segment at a time
        wrapped in DisclosureGroup when collapsible
```

### Engine: one converter, two RPC callers

A new independent crate **`boss-transcript-markdown`** at **`tools/boss/engine/boss-transcript-markdown/`** owns all transcript→markdown rendering. It lives as a subdirectory of `engine/` (consistent with where engine sub-crates land after the pending engine-crate relocation), exposes a `rust_library` target, and has its own `rust_test` target — the same extract-and-re-export pattern introduced by T789 ([PR #919](https://github.com/spinyfin/mono/pull/919)). `boss-engine` depends on it and re-exports it, so existing path references inside the engine stay stable.

The crate's public API:

```rust
pub struct TranscriptSegment {
    pub seq: u64,
    pub role: SegmentRole,        // User | Assistant | Thinking | Tool | System
    pub label: String,            // e.g. "User", "Assistant", "💭 Thinking", "⚙ Bash", "↳ result", "🔗 PR"
    pub timestamp: Option<String>,
    pub model: Option<String>,
    pub markdown: String,         // the rendered body for this one event
    pub collapsible: bool,
    pub default_collapsed: bool,  // thinking + large tool_results start collapsed
    pub truncated: Option<TruncationInfo>,  // { shown_bytes, total_bytes }
}

pub fn events_to_segments(events: &[TranscriptEvent], opts: &RenderOpts) -> Vec<TranscriptSegment>;
pub fn segments_to_markdown(segs: &[TranscriptSegment]) -> String;   // flatten for CLI / single-blob source
```

Two callers, one converter (satisfies the "one converter, two callers" constraint):

1. **`executions.transcript` RPC (app)** returns the structured `Vec<TranscriptSegment>` plus `is_live` / `complete`.
2. **`agents.transcript` RPC serving the CLI** gains a `format` field; `--format=markdown` calls `segments_to_markdown` and prints the flattened document. (The existing CLI `render_text` should move into this module too, so all transcript rendering — text + markdown — lives in one home.)

The app uses the structured form (for laziness/collapsing); the CLI uses the flattened blob. Both come from `events_to_segments`, so fidelity never diverges between surfaces.

#### JSONL → markdown mapping (fidelity table)

| Event | Rendered segment |
|---|---|
| user text | role `User`; body = text. Timestamp in the row header. |
| assistant text | role `Assistant`; body = text; model annotation in header. |
| thinking | role `Thinking`; body = text in a blockquote; `collapsible=true, default_collapsed=true` (verbose, de-emphasized). |
| tool_use | role `Tool`; label = tool name; input as a fenced code block — `Bash` → ```sh of the command; `Edit`/`Write` → file path + fenced contents/diff; everything else → pretty-printed ```json of `input`. |
| tool_result | role `Tool` (`↳ result`); output in a fenced code block; `is_error` flagged; large output truncated to `RenderOpts.max_result_bytes` with `truncated` set and `collapsible=true`. |
| system (`stop_hook_summary`, `turn_duration`, hook payloads) | role `System`; subtype as label; body as a de-emphasized blockquote. |
| `pr-link` (today a `System` subtype) | role `System`, label `🔗 PR`; body = a markdown link to the PR. |
| `hook_success` / PreToolUse / Stop / attachments (today `System`/folded) | role `System`; labeled note; `collapsible=true` when verbose. |
| unknown / malformed | skipped by `parse_transcript` (already); never aborts. |

Conversation order = `seq`. Code blocks are preserved verbatim (the converter never re-wraps inside fences). Timestamp/model are passthrough.

#### Execution resolution — read from `work_executions`, not the live supervisor

The new RPCs resolve transcripts from the **durable `work_executions` row**, sidestepping the "unknown run" divergence entirely:

1. `executions.list { task_id }` → `Vec<WorkExecution>` via the existing `list_executions_for_task` (newest-first). Pure DB read.
2. `executions.transcript { execution_id, format? }`:
   - look up the row → prefer the stored `transcript_path`; fall back to recomputing `~/.claude/projects/<cwd-slug>/<session_id>.jsonl` from `session_id`;
   - if the file is missing → typed `TranscriptUnavailable { reason }` (rotated / GC'd / never-started);
   - else `parse_transcript` → `events_to_segments` → return segments + `is_live` (true when the execution status is running / no `ended_at`) + `complete`.

Keying on `execution_id` (a stable, non-null PK) rather than `run_id` (nullable, supervisor-coupled) is what makes historical and "unknown run" cases just work.

### App: the transcript viewer window

A new value-keyed scene in `BossApp.swift`:

```swift
struct TranscriptViewerRef: Codable, Hashable { var taskId: String; var preselectExecutionId: String? }

WindowGroup(id: "transcript-viewer", for: TranscriptViewerRef.self) { $ref in
    if let ref { TranscriptViewerView(ref: ref) }
}
```

`TranscriptViewerView` is a master/detail split:

- **Left — execution list.** Reuses the existing `ExecutionVM` + `ExecutionRow` (already built for `TaskDetailView`), fed by `executions.list`. Selecting a row loads its transcript. Empty state when the task has zero executions.
- **Right — transcript pane.** `TranscriptView(executionId:)` calls `executions.transcript`, then renders:

  ```swift
  List(segments) { seg in
      if seg.collapsible {
          DisclosureGroup(isExpanded: binding(seg)) {
              Markdown(seg.markdown).markdownTheme(.boss)
          } label: { SegmentHeader(seg) }   // role, label, timestamp, model
      } else {
          VStack(alignment: .leading) { SegmentHeader(seg); Markdown(seg.markdown).markdownTheme(.boss) }
      }
  }
  ```

  This **reuses the one MarkdownUI renderer + `Theme.boss`** (no second markdown renderer — constraint satisfied), but renders **one segment at a time inside a `List`**, so MarkdownUI only builds ASTs for visible rows (laziness — performance goal), and verbose thinking/large results collapse (`DisclosureGroup` — fidelity goal). `MarkdownDocView` is untouched and continues to serve design docs.

- **`EngineClient`** gains the calls: `executionsList(taskId:) -> [ExecutionVM]` and `executionTranscript(executionId:) -> TranscriptDoc` (segments + flags). The generic `.engineText` source already proves the round-trip pattern; this just adds two typed methods.

### Invocation surface

**Both**, justified:

- **Context menu on the task card** (`TaskCardView`) — `Button("View transcripts…")`, mirroring the existing "View design" menu item. This is the fast power-user path (right-click → read) and is where per-task actions already live.
- **Button in the task detail popup** (`TaskDetailView`) — placed next to the *existing* `Section("Executions")` list. Since the detail already lists executions, "View transcripts" there is the natural in-context entry, and clicking a specific execution row can open the window with that execution **preselected** (`preselectExecutionId`).

Two surfaces, one window, negligible extra code (one menu item + one button, both calling `openWindow(id:"transcript-viewer", value:)`). The context menu wins on speed; the detail button wins on context and preselection — they serve different moments, so we ship both.

### Live / partial / missing / empty handling

- **Live (in-progress):** `is_live=true`; render the partial transcript (`parse_transcript` already drops the half-written last line), show a "still running…" banner, and offer a Refresh (or a low-frequency auto-poll reusing the tail's cadence). Never error.
- **Missing/rotated/GC'd:** `TranscriptUnavailable { reason }` → a clear "transcript no longer available" state with the reason, not a crash.
- **Zero executions:** execution list shows an empty state; transcript pane shows a placeholder.
- **Huge single tool_result:** engine truncates to `max_result_bytes` with `truncated` metadata; the collapsed segment shows "(showing N of M KB)". Full-output retrieval (e.g. an `executions.transcript` segment-range param) is a follow-up if operators ask for it.

## Risks / open questions

- **MarkdownUI laziness inside `List`/`LazyVStack`.** The performance argument rests on MarkdownUI only building ASTs for visible rows. We should spike this with a synthetic ~500-message transcript before committing — if `List` still over-eagerly materializes, fall back to manual windowing or Alternative C (paging). *Needs a reviewer/spike decision.*
- **Segment granularity vs. fidelity.** Splitting per-event means a single assistant turn that interleaves text + tool_use + tool_result becomes several rows. Is per-event right, or should we group by *assistant turn* (text + its tool calls + results as one collapsible unit)? Per-turn grouping reads more naturally but complicates collapsing. **Recommendation:** per-event for v1 (simplest, matches `seq`), revisit grouping after dogfooding. *Reviewer input wanted.*
- **System-event fidelity.** `pr-link`, `hook_success`, `attachments`, `stop_hook_summary`, `turn_duration` are not first-class parser variants today — they're `System { subtype, body }`. The converter can render them legibly from subtype, but do we want explicit `TranscriptEventKind` variants for cleaner labels/links (esp. pr-link)? **Recommendation:** render from subtype in v1; add explicit variants only if labeling proves lossy. *Reviewer input wanted.*
- **`executions.list` vs. embedding in task detail.** `TaskDetailView` already gets executions embedded in its detail RPC. Should the viewer reuse that payload or call a dedicated `executions.list`? **Recommendation:** add `executions.list` as the single source so both surfaces share one RPC (and the viewer window doesn't depend on a detail fetch), but this duplicates data the detail RPC already returns — confirm we're fine with that.
- **CLI scope.** The brief asks to extend `bossctl agents transcript <run-id> --format=markdown`. That command is run-id/supervisor-keyed and inherits the "unknown run" gap for finished runs. Do we (a) only add `markdown` to the existing command (accepting the live-only limitation for the CLI), or (b) also add `bossctl executions transcript <execution-id> --format=markdown` backed by the durable resolution? **Recommendation:** (a) for this project (matches the brief; one converter still shared), file (b) as a small follow-up. *Reviewer input wanted.*
- **Window identity.** The window is keyed by `taskId`, so re-invoking "View transcripts" for the same task focuses the existing window (good). But `preselectExecutionId` is part of the key — opening the same task with a *different* preselection would spawn a second window. **Recommendation:** key the window on `taskId` only and pass preselection out-of-band (or ignore it once a window exists). *Minor; confirm.*
- **Auth/PII.** Transcripts can contain secrets the agent saw (tokens in tool output, file contents). This window surfaces them to anyone at the operator's machine. Out of scope to redact, but worth a reviewer ack that this is acceptable for a local power-user tool.

## Proposed implementation task breakdown

Named, PR-sized, with effort hints and explicit dependencies. Each is independently reviewable; the doc PR (this) is the parent.

1. **`engine: jsonl→markdown transcript converter (independent crate)`** — *Effort: M. Deps: none.*
   New crate `boss-transcript-markdown` at `tools/boss/engine/boss-transcript-markdown/`: own `Cargo.toml`, `BUILD.bazel` (`rust_library` + `rust_test`), and `src/lib.rs`. Implements `TranscriptSegment`, `RenderOpts`, `events_to_segments`, `segments_to_markdown`; the full mapping table (incl. Bash/Edit/Write special-casing, tool_result truncation, system/pr-link rendering). Move the CLI's `render_text` into this crate so text+markdown share a home. Unit tests (in the crate's own `rust_test` target) over fixture JSONL covering every event kind. `boss-engine`'s `BUILD.bazel` and `Cargo.toml` gain a dep on this crate; `engine/src/lib.rs` re-exports it (`pub use boss_transcript_markdown as transcript_markdown;`) so in-engine references stay stable. Bazel visibility: `//tools/boss/engine:__pkg__` only. (Optionally fold in explicit parser variants for pr-link/hook_success/attachments — otherwise render from `System.subtype`.)

2. **`engine: executions.list + executions.transcript RPCs (+ CLI --format=markdown)`** — *Effort: M. Deps: 1.*
   Expose `executions.list { task_id }` (wraps existing `list_executions_for_task`) and `executions.transcript { execution_id, format? }` (resolve via `work_executions.transcript_path`/`session_id`, parse, call the converter, return segments + `is_live`/`complete`, typed `TranscriptUnavailable`). Add `Markdown` to the `bossctl agents transcript --format` enum, routed through `segments_to_markdown`. Tests for live/missing/zero/normal.

3. **`app: transcript viewer window + execution list`** — *Effort: M. Deps: 2 (RPCs; can stub).*
   New `transcript-viewer` `WindowGroup` keyed by `TranscriptViewerRef`; `TranscriptViewerView` master/detail; left pane reuses `ExecutionVM`/`ExecutionRow` fed by `executions.list`; selection + empty state. `EngineClient.executionsList` + `executionTranscript`.

4. **`app: lazy segmented transcript renderer`** — *Effort: M–L. Deps: 2, 3.*
   `TranscriptView`: `List` of segments, each rendered via the existing `Markdown(...).markdownTheme(.boss)` component; `DisclosureGroup` collapsing for thinking/large results; segment headers (role/label/timestamp/model); truncation affordance; jump-to-turn/heading navigation. Includes the MarkdownUI-laziness spike from Risks before finalizing.

5. **`app: invocation surfaces (card context menu + detail button)`** — *Effort: S. Deps: 3.*
   `Button("View transcripts…")` in `TaskCardView`'s context menu; a button next to `TaskDetailView`'s `Section("Executions")`; wire execution-row click → `openWindow` with `preselectExecutionId`.

6. **`app/engine: graceful states + live refresh`** — *Effort: S. Deps: 3, 4 (can fold into them).*
   "Still running" banner + Refresh/low-freq poll for `is_live`; "transcript unavailable" state for `TranscriptUnavailable`; zero-execution empty state polish. Bundle into 3/4 if small.

Suggested order: **1 → 2 → (3 ∥ 4) → 5 → 6**, with 3 and 4 stackable behind stubbed RPCs.

(end)
