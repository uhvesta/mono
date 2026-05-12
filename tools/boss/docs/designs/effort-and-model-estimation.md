# Boss: Effort-Level Taxonomy and Dispatch Semantics

## Problem

Every Boss worker spawns the same way today. `engine/src/runner.rs:279` writes `claude "$(cat .claude/initial-prompt.txt)"` into the worker pane's pty regardless of work item — no `--model`, no env overrides, no per-task tuning. The shell falls through to whatever the human's `~/.claude/settings.json` defaults to (currently Opus 4.7) with the full default token budget and no per-task hint about how thorough to be.

That uniformity is wrong in both directions. A scan of the last few weeks of chores in this product (see Appendix A) shows two persistent failure modes:

- **Trivial chores running on Opus.** "Apply PR #357 resize-cursor fix to the left nav bar divider," "Clear search filter when switching from Fencers to Results," "macOS: add bottom padding under kanban swimlanes to match top padding," and "boss CLI: infer --product from globally-unique ids" are all single-file, near-mechanical edits. Spawning Opus for them burns budget and adds latency a Haiku spawn would not. Several of these chores land in well under five minutes of human time; the cost differential between Haiku 4.5 and Opus 4.7 for those runs is the difference between rounding-error and meaningful.
- **Genuinely large investigations getting the same defaults as the trivials.** "Investigate: isolated test instance of Boss + engine (separate DB, logging, sockets; cube shared OK)" is a 6.5 KB description that explicitly names "investigate" twice and spans backend + CLI + ops surfaces. "Engine WorkerPool releases slot before pane is torn down" is 8.4 KB and asks the worker to chase a race condition across the engine/app boundary. There is currently no way to bias these toward more thorough effort (longer planning step, bigger reasoning budget, willingness to fan out to subagents). They get the same defaults as the one-line CSS tweaks.

This document pins down the *effort-level taxonomy*, the *dispatch knobs each level controls*, the *model_override* contract, and the *heuristic* the Boss coordinator (this session) uses to attach an estimate to chores and tasks as it files them.

Three sibling tasks consume this design:

- `task_18aebf113fe16d20_e` — schema + CLI surface for `effort_level` and `model_override`.
- `task_18aebf12c23f1ff0_f` — dispatcher: honour both at worker spawn.
- `task_18aebf144b365660_10` — Boss coordinator: attach estimates at creation time.

This doc is the contract those three tasks implement against.

## Goals

- A four-value `effort_level` enum that maps cleanly onto observed work distribution.
- A small, well-motivated set of dispatch knobs each level controls — no invented levers.
- A `model_override` field with a defined slug shape and a precedence rule.
- A deterministic, human-auditable heuristic for the coordinator to apply at creation time.
- A non-blocking escalation path for "this is bigger than estimated" that workers can take in a later increment.
- A first version that ships against today's runner (`engine/src/runner.rs`) — no rewrite of the spawn flow.

## Non-Goals

- **Learned / ML-based estimation.** The first cut is a heuristic plus explicit `--effort` and `--model` flags. Whether to replace the heuristic with a learned classifier is a follow-up, not v1.
- **Mid-task model swapping.** The estimate is fixed at spawn. A worker that discovers it is bigger than estimated can flag for re-classification, but the *current* run keeps the model it started with.
- **Per-product cost budgets or quotas.** No "this product has $X/day to spend" logic. The dispatcher just honours the level on the row and moves on.
- **Per-stage models inside one worker run.** No "Haiku for planning, Opus for execution" — one worker, one model.
- **Replacing the engine's existing defaults for non-effort-tagged rows.** Existing rows with no `effort_level` continue to dispatch exactly as they do today. Migration is a no-op.
- **Scoring projects.** `effort_level` lives on rows that get dispatched (tasks, chores, `project_task`), not on projects. A project is implicitly the union of its tasks.

## Naming

- New column: **`effort_level`** on the `tasks` table. Enum: `trivial | small | medium | large | max`. Stored as TEXT for legibility in `sqlite3` dumps; constrained in code, not by a `CHECK`. `trivial`–`large` are the four scope-flavored values the coordinator's heuristic emits; `max` is an explicit escape-hatch the heuristic never assigns (Q1).
- New column: **`model_override`** on the `tasks` table. Nullable TEXT containing a *model slug*. Empty / NULL means "no override."
- Model slugs are the same strings `claude --model` accepts: `claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`, and the short aliases `opus`, `sonnet`, `haiku`. The dispatcher does not invent new slugs; the worker is what consumes them and `claude` is the source of truth on what resolves.
- CLI flags: `--effort {trivial|small|medium|large|max}` and `--model <slug>` on `boss chore create`, `boss task create`, and the chore/task `edit` verbs.
- Heuristic output type (in the coordinator): **`EffortEstimate { level, confidence, reasons }`**. `reasons` is a short list of strings ("description length > 4 KB", "matches `investigate` marker") so the coordinator can explain its choice in the chore's creation message.

---

## Design Question 1 — The `effort_level` Enum Values

### Options

- **(a) Three values: `small | medium | large`.** Matches t-shirt sizing. Concise.
- **(b) Four scope-flavored values plus a max escape hatch: `trivial | small | medium | large | max`.** Four levels the coordinator can pick from based on scope; `max` reserved for explicit human override.
- **(c) Adopt Claude's effort vocabulary directly: `low | medium | high | xhigh | max`.** Removes any translation layer; the column stores exactly what `claude --effort` expects.
- **(d) Numeric 1–5.** Maximum flexibility, zero memorability.

### Decision

**(b) — `trivial | small | medium | large | max`.**

Three is too few: the `trivial` cases (one-line CSS fixes, dependency bumps, apply-the-same-fix-elsewhere chores) are common enough and cheap enough that conflating them with `small` loses the very signal we are introducing the enum to capture. The Haiku-vs-Sonnet decision happens right at the `trivial`/`small` boundary; collapsing them removes the boundary.

Adopting Claude's vocabulary directly (option c) is genuinely attractive — it removes the level-to-effort translation layer in the dispatcher and keeps the column legible against `claude --help`. We reject it for one specific reason: the coordinator's heuristic in Q4 is fundamentally *scope-classifying* ("description ≥ 4 KB → large", "matches `investigate` marker → large"). The labels `trivial / small / medium / large` carry that scope meaning faithfully; `low / medium / high / xhigh` carry token-spend meaning, which is one inference away from scope. Keeping the column scope-flavored makes the heuristic's reasons strings ("estimated `large` — investigate marker matched") read naturally to a human reviewing a chore. The mapping to Claude's effort values is a single small table in the dispatcher (Q2); we pay that cost once.

`max` is added as a fifth value with a deliberately different naming pattern: it is *not* a scope description, it is an escape hatch. The coordinator's heuristic in Q4 never emits `max` — only humans set it, via `--effort max` on the CLI. Use cases: "this is genuinely frontier work and the cost is justified," or "the human evaluated this carefully and wants Opus 4.7 at maximum effort regardless of what the markers suggest." Reserving `max` here (rather than only as a `--model`-side override) keeps the effort axis self-contained: humans don't have to think about model overrides when what they really want is more reasoning depth.

Numeric is rejected on legibility grounds — `effort_level = 'small'` in a SQLite dump is self-documenting; `effort_level = 2` is not.

### Concrete examples per level (drawn from real chores)

`trivial` — minutes of work, single file, no design judgement:
- "Apply PR #357 resize-cursor fix to the left nav bar divider."
- "macOS: add bottom padding under kanban swimlanes to match top padding."
- "Fix excess gap below kanban lanes — match nav bar gap."
- "Clear search filter when switching from Fencers to Results."

`small` — a self-contained PR, one to a few files, no architectural judgement:
- "Picard header avatar: render as bare sprite, not circle-cropped giant head."
- "boss CLI: infer --product from globally-unique ids instead of requiring the flag."
- "macOS: Product picker shows archived products — filter status != archived."
- "Kanban: hide per-card project badge when grouping by project."
- "Resolve merge conflicts on mono#326."

`medium` — multi-file change with real design judgement, but well-scoped:
- "Add created_via provenance to chore/task creates."
- "macOS: hide sidebar-toggle on Agents tab + flush tab selector to the left after window decorations."
- "Live status: transcript_path never set on slots — fix events-socket → set_run_transcript_path_if_unset path."
- "PR #361: replace AppKit cursor overlay with .pointerStyle(.frameResize) (macOS 15+)."

`large` — investigation, architectural decision, or work that explicitly spans multiple subsystems:
- "Investigate: isolated test instance of Boss + engine (separate DB, logging, sockets; cube shared OK)."
- "Engine WorkerPool releases slot before pane is torn down → engine/app drift → SpawnWorkerPane returns SlotBusy on slot reuse."
- "Audit macOS app for simplifications enabled by raising deployment target."
- "Drag-to-Doing dispatch — read existing JSONL stream, land deferred reader verbs, analyze the two real failure modes."
- "Instrument live_status pipeline end-to-end — diagnostics, not another speculative fix."

`design`-kind tasks (per [`design-producing-tasks`](design-producing-tasks.md)) are implicitly `large` — a design doc is an investigation by definition. The coordinator may still attach an explicit `effort_level = 'large'` for clarity, but the dispatcher treats unset-on-design as `large`.

`max` deliberately has no example list. By construction it is reached only via explicit `--effort max` on the CLI; if you find yourself wanting an example of "a `max` chore," you almost certainly want `large` and trust the dispatcher to pick `xhigh` effort. Use `max` when the human's evaluation says the chore is worth Claude's absolute-maximum reasoning depth — frontier debugging across many subsystems, design synthesis under unusual ambiguity, the kind of case where the doc on Claude's `effort` parameter says "reserve for genuinely frontier problems."

---

## Design Question 2 — What Each Level Controls at Dispatch Time

### The candidate knob list

The project description names five candidates: default model, per-execution token cap, default subagent depth, whether the worker plans before executing, default timeout. We add one more: a small per-level prompt addendum.

For v1, we keep **only the knobs whose configuration the dispatcher actually has a clean path to set today** — and we prefer Claude's native controls to anything we'd reinvent. Anything that requires a new RPC to the worker, or a new claude-side feature, is filed as a follow-up.

### Use Claude's native effort parameter, not env-var token caps

An earlier draft of this doc set `CLAUDE_CODE_MAX_OUTPUT_TOKENS` and `MAX_THINKING_TOKENS` env vars on the worker subprocess to bound spend per level. That was reinventing something Claude already exposes natively: the **`output_config.effort`** API parameter, documented at <https://platform.claude.com/docs/en/build-with-claude/effort>. The accepted values are `low | medium | high | xhigh | max`. Crucially, effort affects *all* output tokens — text, tool calls, and (for adaptive-thinking models) extended thinking — without us having to hand-roll separate token caps. The Claude docs explicitly recommend effort as the replacement for `budget_tokens` on Opus 4.6 / Sonnet 4.6, and as the canonical control on Opus 4.7's adaptive thinking.

The `claude` CLI (Claude Code) exposes this as `--effort <level>` with the same five values (`claude --help`). Today's spawn line, `claude "$(cat .claude/initial-prompt.txt)"` at `engine/src/runner.rs:279`, becomes `claude --model <slug> --effort <level> "$(cat .claude/initial-prompt.txt)"`. One CLI flag, one API parameter, no env-var token caps anywhere in the dispatch path.

### What the dispatcher wires per level

Two knobs come out of `effort_level`, plus one independent knob from `model_override`:

| Knob | Source | How the dispatcher applies it |
|---|---|---|
| **Claude effort** | `effort_level` (Q1 enum) → Claude effort value via the mapping table below | `claude --effort <claude-value>` |
| **Model** | Q3 precedence: `model_override` → effort default → product default → engine default | `claude --model <slug>` |
| **Prompt addendum** | `effort_level` (Q1 enum) | Appended to `.claude/initial-prompt.txt` before the existing prompt body |

The prompt addendum is explicitly **additive and secondary** to the native effort signal. Claude's `--effort` does the heavy lifting on how thoroughly the worker reasons and how many tokens it spends; the addendum is a light nudge on *behaviour* — "sketch a plan first" — that complements but does not replace the native control. If a level's addendum is `none`, the prompt is unchanged from today.

### The chosen mapping

| Level | Default model | `claude --effort` | Prompt addendum |
|---|---|---|---|
| `trivial` | `claude-haiku-4-5-20251001` | `low` | none — direct execution |
| `small` | `claude-sonnet-4-6` | `medium` | none |
| `medium` | `claude-sonnet-4-6` | `high` | "Sketch a brief plan before you start editing." |
| `large` | `claude-opus-4-7` | `xhigh` | "Begin with a written plan. Identify the files you expect to touch and the order you'll touch them in. Confirm the approach against the work item's description before writing code." |
| `max` | `claude-opus-4-7` | `max` | "Begin with a written plan. Identify the files you expect to touch and the order you'll touch them in. Confirm the approach against the work item's description before writing code." |

A few notes on the table:

- **The effort mapping follows Claude's published guidance.** The Claude docs recommend `medium` as the Sonnet 4.6 default and `xhigh` as the Opus 4.7 starting point for coding/agentic work; the table aligns with that. `trivial → low` matches Claude's "simpler tasks, lowest cost" use case for Haiku.
- **Model defaults bracket the price/latency curve.** Haiku for `trivial`, Sonnet for the middle, Opus for `large` and `max`. The boundary between `small` and `medium` is on Sonnet specifically because the difference there is *effort + prompt*, not model class.
- **`max` and `large` share a model and prompt addendum** — they diverge only on the effort value. That captures the intent of `max`: "treat this as a `large` row but explicitly authorize Claude to spend up to its maximum reasoning depth."
- **Prompt addenda are concatenated to the existing spawn prompt** in the path that writes `.claude/initial-prompt.txt` (currently `runner.rs:272`). They are not template-replacements of the existing prompt — the existing prompt's task-implementation framing stays. If a level's addendum is `none`, the prompt is byte-identical to today.

### Knobs explicitly *not* in v1

- **Per-level env-var token caps** (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_THINKING_TOKENS`). Superseded by `--effort`. Claude's effort parameter already affects all output tokens including extended thinking; the env vars would either fight the effort signal or be redundant with it. If we ever observe a level whose `--effort` setting gives the wrong shape of spend, raise it as a re-tune of the table above, not as a parallel cap mechanism.
- **Default timeout.** The engine already has a worker-watchdog story (independent of effort); a separate per-level timeout adds a second escape hatch we do not need. Re-evaluate after we have data from a few weeks of `large` runs.
- **Default subagent depth.** Claude already lets the worker spawn subagents at its own discretion, and the effort parameter already biases the model toward fewer tool/subagent calls at lower levels. The right additional knob would be an explicit *budget* on subagent spawns, not a depth limit, and we do not have that surface yet. Filed as a follow-up.
- **Per-execution wall-clock cost cap.** Out of scope for v1; this is the per-product budget non-goal.

---

## Design Question 3 — `model_override` Shape and Precedence

### Shape

`model_override` is a nullable string. Allowed values are slugs `claude --model` accepts. We do **not** validate slugs at write time — claude's accepted set changes as new models ship and we do not want the engine to be the bottleneck on adopting a new model. The dispatcher passes the slug through to `claude --model <slug>` and surfaces whatever error claude returns if the slug is bad.

Concretely, today the accepted slugs are:

- Full IDs: `claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`.
- Short aliases: `opus`, `sonnet`, `haiku`.

The CLI surface autocompletes against the short aliases; the column stores whatever the user passed.

### Precedence

When the dispatcher picks the model for a worker, it resolves in this order, taking the first non-empty value:

1. **`tasks.model_override`** — explicit per-row override.
2. **Effort-level default** — `trivial → haiku`, `small / medium → sonnet`, `large / max → opus` (per Q2).
3. **Product default** — `products.default_model` (new column, nullable). Lets a product owner say "default everything on this product to Sonnet."
4. **Engine default** — whatever `claude` resolves to with no `--model` flag (currently Opus 4.7).

The effort value and the prompt addendum follow `effort_level` *only*; `model_override` does not change them. The rationale: a user who overrides to Haiku on a `medium` row is asking "use Haiku for this one," not "treat this as a trivial." If they want a `trivial`-shaped run (Haiku at `low` effort, no addendum), they set `effort_level = 'trivial'`.

### What lives where

| Field | Table | Set by |
|---|---|---|
| `effort_level` | `tasks` | Coordinator at creation; CLI `--effort` at create / edit. |
| `model_override` | `tasks` | CLI `--model` at create / edit. The coordinator does **not** set this from the heuristic. |
| `default_model` | `products` | CLI `boss product set-default-model <slug>`. |

The coordinator does not write `model_override` because the model decision is a *property of the level*, and the coordinator's job is to estimate the level. Writing both would couple them. If a chore should run on Haiku, that is because it is `trivial`; the heuristic should reach that conclusion via the level, not by also setting `model_override = 'haiku'`.

---

## Design Question 4 — The Coordinator's Estimation Heuristic

### Inputs

The coordinator has access to, at chore/task creation time:

- The **title** (`tasks.name`).
- The **description** (`tasks.description`).
- The **kind** (`chore`, `task`, `project_task`, `design`).
- The **parent project's description**, if any (for `project_task` rows).
- Implicit knowledge of the product the row belongs to.

It does **not** have access to repo structure, file paths the work might touch, or historical run data. The heuristic is text-only.

### Outputs

`EffortEstimate { level, confidence: low | medium | high, reasons: Vec<String> }`. The coordinator stores `level` on the row and posts `reasons` as a short message into the chore's transcript when it files it ("Estimated `small` — single-file marker matched; description under 1.5 KB; no investigation marker.").

The heuristic emits only `trivial | small | medium | large`. It never assigns `max`; that level is reserved for explicit human invocation via `--effort max` (per Q1). If the coordinator believes a chore warrants more effort than `large`, the right action is to surface that recommendation in the reasons string and let the human upgrade to `max`, not to make the call automatically.

### The rules

Evaluated top-to-bottom; first matching rule wins:

1. **Design-kind rows → `large`.** Confidence high. Reason: "design kind."
2. **Title or description matches an `investigate` family marker** → `large`. Confidence high. Markers: `investigate`, `audit`, `instrument`, `diagnose`, `end-to-end`, `root cause`, `architect`, `redesign`, `migrate`, `rearchitect`. Reason: lists the matched markers.
3. **Description ≥ 4 KB** → `large`. Confidence medium. Long descriptions are almost always projects in disguise. Reason: "description size N KB."
4. **Title or description names a multi-file or multi-subsystem hint** → `medium`. Confidence medium. Hints: `+` between subsystems ("engine/src/ + cli/"), "across", "spans", multiple module names from the same path-prefix vocabulary (`engine`, `cli`, `protocol`, `app-macos`, `cube`, `bossctl`). Reason: lists the matched hint.
5. **Title matches a mechanical-edit marker** → `trivial`. Confidence high. Markers (full word, case-insensitive): `rename`, `apply`, `revert`, `bump`, `move`, `delete`, `remove`, `hide`, `show`, `pad`, `align`, `re-export`, `gap`, `cursor`, `badge`, `tooltip`. Reason: lists the matched marker. (These markers were chosen by scanning the chore corpus in Appendix A; they correlate strongly with one-file edits.)
6. **Description < 500 bytes and title is one clause** → `trivial`. Confidence low. Reason: "short description, single-clause title."
7. **Description < 1500 bytes and no other rule fired** → `small`. Confidence low. Reason: "short description, no large/medium markers."
8. **Otherwise** → `medium`. Confidence low. Reason: "fallback."

### Examples (from Appendix A)

| Row | Matched rule | Level |
|---|---|---|
| "Apply PR #357 resize-cursor fix to the left nav bar divider." (1042 B desc) | Rule 5 (`apply`, `cursor`) | `trivial` |
| "Investigate: isolated test instance of Boss + engine ..." (6497 B desc) | Rule 2 (`investigate`) → also Rule 3 backup | `large` |
| "boss CLI: infer --product from globally-unique ids" (1751 B desc) | Rule 7 | `small` |
| "Engine WorkerPool releases slot before pane is torn down..." (8442 B desc) | Rule 3 | `large` |
| "Add created_via provenance to chore/task creates." | Rule 4 (multi-surface: cli + engine + schema) | `medium` |
| "Instrument live_status pipeline end-to-end..." | Rule 2 (`instrument`, `end-to-end`) | `large` |
| "Fix excess gap below kanban lanes — match nav bar gap." | Rule 5 (`gap`) | `trivial` |

The list is deliberately auditable: every level the coordinator picks comes with a one-line reason a human can review on the chore.

### Where it runs

The heuristic runs in the coordinator's chat handler — i.e. *here*, in this Boss session — at the moment the coordinator calls `boss chore create` or `boss task create`. It is **not** an engine-side computation. The engine just stores whatever the CLI tells it.

This matters because the heuristic is *prompt-driven* (the coordinator can read the human's intent context) and not purely string-matching. The rules above are the *minimum* the coordinator commits to; the coordinator may override its own match with explicit reasoning ("the description is under 1.5 KB but the human said 'this is going to be a big one,' so calling it `large`"). The explicit `--effort` flag on `boss chore create` is the override surface for the human; the coordinator-as-LLM is the override surface for nuance the rules miss.

### Edge cases

- **Re-classification at edit.** If the human edits the description and the row's `effort_level` is still `unset` or matches the prior heuristic, the coordinator re-runs the rules. If the level has been hand-set, edits do not silently re-classify.
- **Empty description.** Default to `small`. Confidence low. Reason: "empty description; safe default." (Not `trivial`, because we'd rather err toward Sonnet than Haiku on a row the human did not explain.)
- **`project_task` under a project.** Inherit hints from `projects.description` if the task's own description is short. Use the longer of (project description, task description) for the size checks.

---

## Design Question 5 — Escalation Path

A worker mid-run discovers the work is bigger than the estimate. What happens?

### Decision

**First cut: Stop-boundary signal, no in-flight model swap.**

When a worker decides it needs to escalate, it writes a structured marker on a `Stop` hook boundary that the coordinator reads on probe. The marker is a single line in the worker's final response, in a format the coordinator already parses for other Stop-boundary signals:

```
[effort-escalation] requested_level=large reason="ran into a multi-subsystem race; rule-3 missed because the description didn't mention engine/app boundary"
```

The coordinator's probe handler reads this, updates the chore row's `effort_level`, posts a comment back to the worker ("acknowledged; the next dispatch on this chore will use the new level"), and lets the current run finish on its existing model. The next dispatch for this chore — whether because the worker hit a deferred TODO and is being re-spawned, or because the human reset the chore to `todo` and re-runs it — picks up the new level.

### Why not swap mid-flight

Three reasons:

- **Claude's session is bound to one model.** Mid-task model swapping requires either a new `claude` feature or terminating the worker and restarting from scratch. The first is out of our control; the second loses everything the worker has accumulated in context. Neither is worth it for the rare "I underestimated this" case.
- **Estimation accuracy is the larger problem.** If escalation is common, the heuristic is wrong, not the dispatch. The Stop-boundary signal *also* feeds back into the heuristic — the coordinator records each escalation against the row's original markers and the human can review which markers under-classified.
- **The cost of finishing a `medium` on a Sonnet that should have been Opus is bounded.** Worst case the worker stops short and asks for help. That is fine; the human re-runs with the new level. The cost of *aborting* a partial run to swap models is unbounded — context is lost, tools have to be re-run, the user pays for both worker invocations.

### Out of scope for this design

- **The coordinator's logic for processing the escalation signal beyond updating the level.** Filed under the sibling coordinator task (`task_18aebf144b365660_10`).
- **Worker-emitted *de-escalation* signals** ("this was easier than estimated, I'm done early"). Useful for the heuristic feedback loop but not load-bearing for v1. Filed as a follow-up.
- **Cross-row escalation** ("this medium chore is actually part of a large project I just realised existed"). Out of scope; the human files the project and re-parents.

---

## Open Questions

These are deliberately *not* answered in v1, to keep the implementation tasks narrow.

1. **Do we expose `effort_level` on the kanban card?** Probably yes (a small badge), but the visual design is the macOS app's call. Filed against the existing kanban design surface.
2. **Is the Q2 effort mapping right per model?** `medium → high` on Sonnet and `large → xhigh` on Opus 4.7 follow Claude's published recommendations, but the mapping is a guess for our specific workload distribution. The dispatcher task should make the table trivial to tune without a schema change (e.g. read from a constants module). Re-evaluate after a few weeks of real runs by looking at which levels stall, escalate, or finish underspending.
3. **Does `effort_level` carry across when a worker spawns subagents?** The subagent inherits the worker's claude session and therefore its model and effort; we do not have a separate dispatch path for subagents. No action needed unless we add one.
4. **Should `model_override` accept a *list* of fallbacks?** ("Try Sonnet; if it 5xxes, drop to Haiku.") Not needed today; `claude` has its own fallback logic via `--fallback-model`. Revisit if we see API-side outages biting in practice.
5. **Should we add a `boss product set-default-effort`?** Symmetric with `set-default-model`. Not in v1 because no product has yet asked for a default that disagrees with the heuristic. File if we see a product whose chores systematically classify wrong out of the box.

---

## Risks

**R1 — The heuristic is wrong for this product.** The marker lists in Q4 were derived from a few weeks of chores on `mono`. A new product (say `flunge`) may have a totally different vocabulary; "rename" on `flunge` may be a 12-file API migration rather than a one-file edit. Mitigation: the rules are auditable and the `--effort` flag exists. As we onboard new products, we add product-specific marker tables or accept that `--effort` will be hand-set more often for that product until the heuristic catches up.

**R2 — `model_override` slugs drift.** A row written today with `model_override = 'claude-opus-4-7'` becomes invalid the day Opus 4.8 ships and 4.7 retires. Mitigation: store the slug verbatim; on the day of retirement claude will surface an error; the human edits the row or clears the override. We do not silently rewrite stored slugs.

**R3 — Effort-level mistuning under-scopes legitimate work.** Claude's effort parameter is a behavioural signal, not a strict cap, but a worker spawned at `--effort low` will systematically explore less than the same chore at `--effort high`. If our `trivial → low` and `small → medium` defaults are too aggressive for some chore shape, the worker stops short. Mitigation: the dispatcher logs the effort value it set; the existing dispatch instrumentation ([`engine-dispatch-instrumentation`](engine-dispatch-instrumentation.md)) surfaces this on the live-status pane. If we see workers stalling at lower effort levels, retune the Q2 table or escalate via Q5.

**R4 — Haiku is not capable enough for a "trivial" we labelled wrong.** A misclassified `trivial` running on Haiku produces a worse result than the same chore on Sonnet would. Mitigation: the Stop-boundary escalation path (Q5) covers this; the worker stops short, the coordinator re-classifies, the next dispatch uses Sonnet.

**R5 — Coordinator-side estimation is non-deterministic.** Because the heuristic runs inside an LLM session (this one), the same chore could in principle get a different level on different days. Mitigation: the *rules* in Q4 are explicit and the coordinator commits to following them as the *minimum*; the reasons string makes the decision auditable. A human who disagrees overrides with `--effort`.

**R6 — Estimation surface adds friction at chore-creation time.** Asking the coordinator to attach an estimate to every new chore could slow chore creation by a few seconds of LLM time per call. Mitigation: the heuristic is cheap (string matching plus an LLM judgement call only when the rules don't decisively match); the cost is negligible compared to running the wrong-tier worker.

**R7 — Workers can lie on the escalation signal.** A `trivial` worker that decides it would rather run on Opus emits the escalation marker to force a re-classification. Mitigation: the next dispatch is what changes; the current run finishes on the existing model. The cost is bounded to one wasted re-dispatch. If this becomes a pattern, add a rate limit per chore.

---

## Follow-up Implementation Chores (to enqueue once approved)

The three sibling tasks of this project are exactly the right granularity:

1. **Schema + CLI surface for `effort_level` and `model_override`** (`task_18aebf113fe16d20_e`). Adds `tasks.effort_level`, `tasks.model_override`, `products.default_model`. Adds `--effort` and `--model` flags to `boss chore create`, `boss task create`, and the `edit` verbs. Adds `boss product set-default-model`. Migration is a no-op for existing rows (level stays NULL; dispatcher treats NULL as "fall through to product/engine default" — see Q3).
2. **Dispatcher: honour `effort_level` and `model_override` at worker spawn** (`task_18aebf12c23f1ff0_f`). `engine/src/runner.rs:279` learns to read both fields and:
   - resolve the model per Q3's precedence,
   - construct `claude --model <slug> --effort <claude-value>` instead of bare `claude`, mapping `effort_level` → Claude effort value via Q2's table,
   - append the per-level prompt addendum to `.claude/initial-prompt.txt` when the level has one.
   No env vars are set on the worker process for token caps — Claude's `--effort` handles spend. Surfaces the chosen model, effort value, and level on the dispatch instrumentation stream so they are visible per-spawn.
3. **Boss coordinator: attach estimates at creation time** (`task_18aebf144b365660_10`). The coordinator runs the Q4 heuristic on every `boss chore create` / `boss task create` call it initiates and passes `--effort` accordingly. Posts the `reasons` string into the chore's transcript or initial comment. Does not write `model_override`.

After all three land, two follow-ups become natural:

- **Escalation handler** (the Q5 Stop-boundary path on the coordinator side). Cheap once (3) is in place.
- **Heuristic feedback loop.** Compare each row's original level against any escalation that fired; surface "marker X under-classified Y% of the time" as an audit report. Lets us tune the marker lists per Q4 without code changes.

---

## Appendix A — The Sampled Chore Corpus

The level examples in Q1 and the marker list in Q4 were derived from a scan of all `kind = 'chore'` rows in the local Boss state DB filed in the rolling four-week window leading up to this design (2026-04-12 → 2026-05-12). 40 chores total; the canonical names appear in Q1 grouped by the level the heuristic in Q4 assigns. A few short stats from that scan:

- 11 rows (28%) classify as `trivial`.
- 14 rows (35%) classify as `small`.
- 9 rows (22%) classify as `medium`.
- 6 rows (15%) classify as `large`.

The split is the rough validation that the four-level enum tracks real work distribution: `trivial` and `small` together are the majority, `large` is rare-but-real, and `medium` is the bin where a default-Sonnet decision is most load-bearing. If the distribution had been "85% small / 15% medium," we would have collapsed to a two-level enum and not bothered.
