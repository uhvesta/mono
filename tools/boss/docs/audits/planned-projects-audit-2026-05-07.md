# Boss "planned" projects — implementation audit (2026-05-07)

Audit of the five Boss product projects still flagged `status=planned` from late April / early May, against what is actually in the `spinyfin/mono` tree today. Investigation only — no taxonomy or product-code changes were made.

## Summary

| ID | Project | Implementation status | Recommended action |
|----|---------|----------------------|--------------------|
| proj_18a1c4df209fdd68_2 | Screenshot upload in chat window | `not_started` | Keep planned; rewrite description with current state of the chat composer |
| proj_18a2bb78815be670_3 | First-class markdown support with inline commenting | `partial` | Split: mark assistant-message markdown done, retain a smaller "doc viewer + inline comments" project |
| proj_18a2bb9a1f7068d8_4 | Planner agent for project planning and task extraction | `partial` | Rewrite description to track only what was *not* absorbed by `project_design` executions, then mark in_progress |
| proj_18a2bbab4bc68d70_5 | Task and project dependency system | `partial` | Rewrite description to reflect declared-dependency scope, mark in_progress |
| proj_18a2bbe20fc03718_8 | Multi-repo and cross-product work modeling | `not_started` | Keep planned; description still accurate |

---

## proj_18a1c4df209fdd68_2 — Screenshot upload in chat window

- **Status**: `not_started`.
  - Wire protocol carries chat as text-only — `FrontendRequest::Prompt { agent_id, text }` with no image / attachment field (`tools/boss/protocol/src/wire.rs:179`).
  - The chat data model is text-only — `ChatMessage` carries a `String` `text` and nothing else (`tools/boss/app-macos/Sources/Models.swift:47`); the transcript enum is `.message | .terminal` only (`Models.swift:68`).
  - The chat composer is a plain `ComposerTextView` with a `Binding<String>` and a paperplane send button — no attach affordance, no `dropDestination` for images, no `NSPasteboard` image-type wiring (`tools/boss/app-macos/Sources/ContentView.swift:393` and the only `paste(nil)` site at `ContentView.swift:1696`, which forwards to `NSText.paste` for plain text).
  - Asset/image references in the SwiftUI tree (`Image(nsImage: portrait)` etc.) are all crew portraits and SF Symbols — none of them render user-pasted screenshots.
  - No PR in `spinyfin/mono` matches `screenshot`, `image`, `paste`, `attach`, or `upload` in a chat-input context. The closest hits are crew-portrait PRs (`#208`, `#224`, `#230`).
- **Scope drift**: description is still accurate, but the *premise hint* — "the user is actively pasting screenshots into chat" — does not match what is actually in the Boss app. The user may be pasting screenshots into Claude Code in worker terminals (which renders images natively); that is a different surface and is not part of this project.
- **Recommended action**: keep `planned`. Rewrite description to (a) drop the "already shipped, find the PR" framing, and (b) be explicit about which surface (Boss agent panel chat? Worker pane chat? Both?) and which input modes (paste, drag-drop, file picker).
- **If partial**: n/a — nothing landed yet.

---

## proj_18a2bb78815be670_3 — First-class markdown support with inline commenting

- **Status**: `partial`.
  - **Markdown rendering for assistant chat messages: shipped.** `MessageRow` renders assistant text via `StructuredText(markdown: message.text).textual` (`tools/boss/app-macos/Sources/ContentView.swift:1755`), backed by the [`textual`](https://github.com/.../textual) Swift package vendored under `tools/boss/app-macos/.build/checkouts/textual`. Supports headings, lists, tables, code blocks, block quotes, math.
  - **Markdown docs viewable in main UI: not_started.** No file-picker, no doc viewer, no plan/README pane in the SwiftUI tree. The `tools/boss/docs/{designs,plans}/*.md` files are not rendered by the app.
  - **Google-Docs-style inline comments: not_started.** No `comment`/`annotation` types in `Models.swift`, no `comment` columns in the SQLite schema (`tools/boss/engine/src/work.rs:1080-1180`), no `Comment*` wire variants in `protocol/src/wire.rs`, no UI for selecting a span and attaching a note.
  - **Comment-driven agent triggering: not_started.** No code path that converts a comment event into an agent prompt.
- **Scope drift**: description conflates "render markdown messages in chat" (already done as a side-effect of the v2 chat work) with "viewable docs + inline comments" (not started). Treating the project as one unit makes its status look more partial than it is.
- **Recommended action**: split.
  - Close out the chat-rendering slice as a one-line "done" item or absorb into the v2 plan rollups.
  - Retain a smaller project with the doc-viewer + inline-comment + comment-triggers-agent scope and its own description.
- **If partial**: candidate follow-up chores
  - Add a `WorkProductDoc` / `WorkPlanDoc` model + wire endpoint exposing repo markdown files under `tools/boss/docs/`.
  - Add a doc-viewer pane to the SwiftUI client that renders one of those docs via `StructuredText`.
  - Add a `comments` table keyed to `(doc_path, anchor_range)` with `created_by`, `body`, `resolved_at`.
  - Add a comment-creation UI (text-selection → "Comment" affordance).
  - Wire a comment → agent-prompt path (probably new `FrontendRequest::PromptFromComment` or a comment-mention syntax that the engine intercepts).

---

## proj_18a2bb9a1f7068d8_4 — Planner agent for project planning and task extraction

- **Status**: `partial`.
  - **Project-level design pass: shipped as an *execution kind*, not as a standalone agent.** `request_execution` for a `Project` work item allocates an execution with kind `project_design` (`tools/boss/engine/src/work.rs:2129-2143`, `work.rs:412-423`). The runner gives that execution a tailored prompt: "draft or update a repo-backed design artifact, identify likely follow-up tasks or phases, stop once the design pass is in a state a human can review." (`tools/boss/engine/src/runner.rs:472-482`). PR #139 ("close out v2 phase 5 — ExecutionCoordinator named deliverables") is the foundational delivery; the prompt-for-kind dispatch is current.
  - **Auto-extraction of tasks from the design output: not_started.** `WorkCompletionService::on_stop` in `tools/boss/engine/src/completion.rs:287-418` only detects a PR and moves the work item to `in_review`. There is no parser that reads the design markdown, extracts task headings, and inserts `Task` rows. The existing flow assumes a human reads the design PR and creates tasks manually (or via `boss task create`).
  - **No "Planner" agent role.** `AgentRole` is only `Standard | Boss` in both Rust (`tools/boss/protocol/src/wire.rs:24`) and Swift (`tools/boss/app-macos/Sources/Models.swift:3`). The "Planner" concept has been absorbed into the `project_design` execution kind on a generic worker, not a dedicated agent role.
- **Scope drift**: original goal was "a Planner agent that produces markdown plans and auto-extracts tasks". What actually shipped is "a project-design execution kind that produces a markdown PR". The "agent" framing is now misleading — `project_design` is just a prompt-template-by-kind on the standard worker.
- **Recommended action**: rewrite description and mark `in_progress`. Specifically:
  - Drop the "Planner agent" framing; rename to "Project-design pass + auto task extraction".
  - Note that the design-pass surface already exists (link to PR #139, `runner.rs:472-482`).
  - Keep the task-extraction goal as the remaining open scope.
- **If partial**: candidate follow-up chores
  - Define a structured-output convention for `project_design` worker runs (e.g. a `## Tasks` section with `- [ ] <task name>` bullets, or a side-car JSON file in the workspace).
  - Extend `WorkCompletionService::on_stop` (or a new `record_design_completion`) to parse that output and call `create_task_in_tx` per bullet, attached to the project.
  - Decide whether tasks land directly as `todo` or through a human review step in the kanban (likely the latter, given the existing `in_review` posture).
  - Surface the extracted-tasks list in the SwiftUI client with an "accept all / reject" affordance.

---

## proj_18a2bbab4bc68d70_5 — Task and project dependency system

- **Status**: `partial`.
  - **Implicit sequential dependencies inside a single project: shipped.** `reconcile_executions` orders a project's tasks by `(ordinal, created_at, id)`, marks the first incomplete one `ready` and the rest `waiting_dependency` (`tools/boss/engine/src/work.rs:453-481`). The `waiting_dependency` execution status is recognised by the kanban and mapped to a UI "Waiting on dependency" state (`tools/boss/app-macos/Sources/Models.swift:335-336`, `tools/boss/protocol/src/types.rs:166`). When `request_execution` is reissued for a `waiting_dependency` row, it is promoted to `ready` (`work.rs:2077`).
  - **Declared dependencies between arbitrary work items: not_started.** No `dependencies` (or `blocked_by`/`blocks`) table in the SQLite schema (`work.rs:1080-1180`), no dependency fields on `Task`/`Project`/`Chore` types in `protocol/src/types.rs`, no wire variants for declaring or removing a dependency edge.
  - **Cross-project / cross-product blocks: not_started.** Same — there is no edge model that could carry these.
- **Scope drift**: original description ("tasks/projects can declare dependencies; dependents are blocked from execution until dependencies complete") is broader than what is in tree. The "implicit sequential" behaviour is real and load-bearing for the kanban, but it is not what the project description sells.
- **Recommended action**: rewrite description and mark `in_progress`. Make explicit what is already done (intra-project sequential auto-dependency via `ordinal`) and what is open (declared dependencies, cross-project edges, project-level dependencies).
- **If partial**: candidate follow-up chores
  - Add a `work_dependencies` table (`from_id`, `to_id`, `created_at`, `kind`) referencing `tasks.id`/`projects.id`/`products.id` (use `classify_id` for type tag).
  - Add wire surface: `CreateDependency`, `DeleteDependency`, plus dependency lists on `Task`/`Project` query types.
  - Extend `reconcile_executions` to also gate on declared dependencies, not just intra-project ordinal.
  - Extend the kanban to render block edges (e.g. a "blocked by N" badge on a Doing card), and refuse drag-to-Doing while incoming edges are unresolved.
  - Decide cycle-detection policy at write time vs. ignore-cycle at reconcile time.

---

## proj_18a2bbe20fc03718_8 — Multi-repo and cross-product work modeling

- **Status**: `not_started`.
  - The data model assumes one repo per product: `Product.repo_remote_url` is a single `Option<String>` (`tools/boss/protocol/src/types.rs:9`, schema `products.repo_remote_url TEXT` in `tools/boss/engine/src/work.rs:1085`).
  - A project always belongs to exactly one product (`projects.product_id NOT NULL REFERENCES products(id)` at `work.rs:1093`); there is no join model or fan-out.
  - Tasks always belong to exactly one product and at most one project (`tasks.product_id NOT NULL`, `tasks.project_id` nullable; `work.rs:1107-1121`).
  - Cube workspace allocation is keyed off the product's single repo remote (`reconcile_executions` at `work.rs:410` reads `product.repo_remote_url` and threads it as the only repo through every execution).
  - The taxonomy design doc explicitly defers this: "Supporting cross-product graphs or multi-repo orchestration beyond a small set of cases" is listed under non-goals (`tools/boss/docs/designs/work-taxonomy.md:45`), with a note to "leave space for future multi-repo support" (line 74) and "future multi-repo support should extend the product model with additional repo links" (line 402). The active plan reiterates the gap (`tools/boss/docs/plans/active/v2-implementation.md:940`: "Multi-product workers; cross-product tasks").
- **Scope drift**: description is still accurate; nothing has been built that would make it stale.
- **Recommended action**: keep `planned`. Description is fine. If the project has been deprioritised in practice, consider lowering its priority rather than archiving — the design docs explicitly call out the placeholder.
- **If partial**: n/a — nothing landed yet.

---

## Cross-cutting observations

- The five "planned" projects do not all decay the same way. Two (Screenshot upload, Multi-repo) genuinely have no implementation and their descriptions still match. Three (Markdown, Planner, Dependencies) have *partial* implementations that landed under different framing — not as the named project, but as side-effects of the v2 kanban / runner work — which is why the staleness is invisible from the project list alone.
- Because of how `reconcile_executions` and the runner already use `kind`-based prompt templating, "Planner agent" and "Dependency system" are best understood as extensions to the *existing* execution-kind / reconcile system rather than greenfield projects. Rewriting the descriptions to land them on those code paths will make the chores actually executable.
- The screenshot-upload hint about the user "actively pasting screenshots into chat" appears to be misattributed — the chat composer has no image path. If the user is pasting screenshots into Claude Code worker panes, that is a Claude-Code feature, not a Boss feature, and it should not be conflated with this project.
