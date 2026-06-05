# Agent-driver abstraction: decouple Boss from Claude Code (capabilities-oriented, mix-and-match)

Project: **P1422**. Foundational prerequisite for [P284 — Copilot CLI as alternative worker backend](copilot-cli-as-alternative-worker-backend.md) and a future Codex backend.

This is a **design doc**, not an implementation. The deliverable is the abstraction; P284 and Codex become thin concrete *drivers* on top of it.

## Goals

- Introduce an engine-side abstraction layer between Boss and the coding-agent CLI it drives, so Boss is no longer hardwired to Claude Code. Every place the engine today shells out to `claude`, parses Claude's transcript, wires Claude's hooks, or renders Claude's `settings.json` becomes a call through a **driver** interface.
- Express the interface as **capabilities** Boss needs from a backend, not as a Claude-shaped contract every backend must satisfy. A driver **declares which capabilities it supports**; Boss adapts (degrade / synthesize / refuse) per capability when one is absent. This is the operator's first hard requirement: *capabilities-oriented, not lowest-common-denominator*.
- Make backend choice **mix-and-match**: per-work-item (and per-product-default) driver selection, recorded on the work item, with Claude Code as one driver among several. This is the operator's second hard requirement: *not a global switch*.
- Re-home the engine's **direct Anthropic API calls** (live-status summarizer, pane-summary, magic-wand, attentions backstop) behind a separate *utility-model* provider seam, so "the worker's driver" and "the model the engine uses for its own background inference" are independent axes.
- Produce a complete, file:symbol-level survey of the coupling (Part 1 below) so the implementation tasks have an exhaustive work-list, not a guess.

## Non-goals

- **Removing or de-privileging the Claude path.** Claude Code is the reference driver and the default. The abstraction must justify itself on the *second*-driver side; the Claude driver is a behaviour-preserving extraction of today's code, byte-for-byte where it spawns a worker.
- **Implementing the Copilot or Codex drivers.** Those are P284 and a later project. This doc defines the seam they plug into and proves the seam fits all three, but writes only the Claude driver.
- **App-side / Swift / macOS changes.** Out of scope. The execution model stays: embed the agent CLI in a ghostty terminal pane. A native UI chat interface is a separate, later project. The kanban already reads abstract `WorkerActivity` / `LiveWorkerState`; nothing app-side needs to know which driver ran.
- **A global cutover or a single `backend` toggle.** Mix-and-match is the end state. There is no env var that redirects all dispatches.
- **Re-deriving the effort/model design.** [`effort-and-model-estimation`](effort-and-model-estimation.md) already centralised the effort→model tables in `effort.rs`. This project makes those tables *per-driver* rather than re-opening the vocabulary.
- **MCP injection, token/quota accounting, cost accounting.** Boss injects no MCP servers today and adds none here. Quota is the backend subscription's concern. Called out as known absences, not v1 work.

## Background — relationship to P284

P284 ("Copilot CLI as alternative worker backend") already exists as a design and answers the *two-backend* version of this question: it forks the engine exactly once, at spawn, into `worker_setup::claude` vs `worker_setup::copilot`, carries a `backend: Claude | Copilot` enum, and writes a parallel hook-normaliser for Copilot's JSON stream. Its Alternative C ("one branch at the bottom, everything else shared") is the right instinct for *two* backends.

This project generalises that instinct for *N* backends and satisfies the capabilities requirement P284 explicitly defers (its Phase 4 names "an optional unifying `WorkerBackend` trait in the engine (the v2 abstraction this design defers)"). The relationship is:

- **P1422 (this doc)** defines the driver trait, the capability set, the absence-policy model, and re-implements Claude on it (behaviour-preserving).
- **P284** becomes the Copilot driver: a `CopilotDriver` implementing the trait, declaring the capabilities it has (and lacking PreToolUse hooks, real-time event socket, five-value effort), riding the absence policies this doc defines. P284's Q1–Q6 decisions (where the choice lives, spawn shape, hook gap, session files, feature-gap matrix, rollout) all survive — they become *the Copilot driver's answers to this interface's capabilities*.
- **Codex** becomes a third driver later, with its own capability declaration.

Where P284 and this doc disagree on a name (P284 calls the column `backend`; this doc proposes `driver`), this doc's choice is canonical because it is upstream — see [Risks / open questions](#risks--open-questions).

---

## Part 1 — Exhaustive survey: where Boss assumes Claude Code

This is the load-bearing deliverable. Every finding is `file:symbol:line` against the current tree (`tools/boss/engine/core/src/` unless noted), with the Claude-specific assumption it bakes in and the *capability* Boss actually needs there. The findings are grouped by the capability they map onto in Part 2.

### 1.1 Worker spawn / dispatch (→ capability: **Spawn**)

| Finding | Location | Claude assumption | Capability Boss needs |
|---|---|---|---|
| Worker invocation string | `effort.rs:SpawnConfig::claude_invocation:137` | Literal `claude --model … [--effort …] [--permission-mode auto\|--dangerously-skip-permissions] [--settings '…'] "$(cat .claude/initial-prompt.txt)"`; trailing newline = pane "return" | "Given a resolved spawn config + workspace + prompt file, produce the command line (or spawn plan) that starts the worker" |
| Spawn line written into pane | `runner.rs:PaneSpawnRunner::run_execution` (≈405–448) | Writes `.claude/initial-prompt.txt`, points worker at `--settings <path>`, unsets `ANTHROPIC_API_KEY` before spawn | "Materialise the initial prompt where the worker reads it; pass driver-specific credentials" |
| Remote spawn wrapper | `engine/core/remote/boss-remote-run.sh` (≈84,159,162) | `command -v claude` health check; `nohup claude --dangerously-skip-permissions "$initial_input"` | "Health-check the driver binary on the remote host; spawn it unattended" |
| Spawn prelude (jj positioning) | `runner.rs` workspace/branch/recovery block (≈1086–1194) | **Driver-agnostic** — jj fetch, branch resume, recovery directives | (no coupling; stays in shared dispatch) |

The prelude splits cleanly: the jj/workspace-positioning part is driver-agnostic and stays in shared dispatch; only the *agent-invocation* tail (the `claude …` line and the `.claude/` file writes) is Claude-specific.

### 1.2 Per-session workspace files (→ capability: **WorkspaceProvisioning**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| `.claude/initial-prompt.txt` path | `runner.rs` (≈405) | Hardcoded dir name + `$(cat …)` read | "A predictable workspace-relative file the worker reads as its first user message" |
| `.claude/` dir + `CLAUDE.md` + `.gitignore` | `worker_setup.rs:write_workspace_files` (≈1247–1285) | Creates `.claude/`, writes `CLAUDE.md`, `.gitignore` (`*`), deletes for fresh state | "A per-session config directory with an agent-rules file, hidden from VCS" |
| Settings file outside workspace | `worker_setup.rs` module doc + `worker_settings_path` (≈15–46, ≈1300) | JSON with `permissions`/`hooks`, passed via `--settings` | "A place to express permission + observability config the worker honours" |
| Global trust pre-seed | `worker_setup.rs:pre_trust_workspace_in_claude_config` (≈1143) | Reads/writes `~/.claude.json`, sets `projects[ws].trusted = true` | "Suppress the backend's first-run workspace-trust prompt" |

### 1.3 Permission model / tool-gating (→ capability: **PermissionPolicy**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| Default permission mode | `worker_setup.rs:render_settings_json` (≈572–574) | JSON `permissions.defaultMode = "auto"` | "Express 'autonomous, honour deny rules' to the backend" |
| Permission flag branch | `effort.rs:claude_invocation` (≈147–150) | Opus ⇒ `--permission-mode auto`; non-Opus ⇒ `--dangerously-skip-permissions` unless corp setting | "Per-model-family autonomy default" |
| Corp-env setting | `settings.rs` (≈37–46) | `workers.non_opus_permission_mode` gate | "Per-install override of autonomy default" |
| Deny rules in Claude syntax | `worker_setup.rs:deny_rules` (≈604–656,682,700) | `Bash(bossctl:*)`, `Read(<state-dir>/**)`, `Edit(**)` etc. in claude rule grammar | "Deny a set of tools/paths (bossctl, state-dir, rm/sudo); reviewer = read-only" |
| Engine-side fs audit (suspenders) | `worker_sandbox_audit.rs` | **Driver-agnostic** — filesystem-level | (no coupling; second line of defence works under any driver) |

### 1.4 Model / effort selection (→ capability: **ModelMenu + EffortMapping**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| Engine default model | `effort.rs:ENGINE_DEFAULT_MODEL:27` | `"opus"` | "A driver-supplied default model" |
| Effort → `--effort` value | `effort.rs:claude_effort_for_level:34` | 5-value `low/medium/high/xhigh/max` | "Effort vocabulary → driver effort knob (Copilot has 3, Codex differs)" |
| Effort → model | `effort.rs:default_model_for_level:61` | `sonnet` / `opus` family aliases | "Per-driver model menu + per-level default" |
| Resolution precedence | `effort.rs:resolve_spawn_config:187` | row override → pool override → effort default → product default → engine default | "Multi-source precedence resolved against the driver's menu" |
| Family classification | `effort.rs:model_is_opus:167` | substring `"opus"` ⇒ Opus | "Driver classifies its own slugs into families for autonomy default" |
| Data model | `protocol/src/types.rs:EffortLevel:1772`, `Task.model_override`, `products.default_model`; migrations `work/migrations_b.rs:318–347` | TEXT columns documented "Claude model slug" (`cli/src/main.rs` help text) | "model_override is backend-scoped; **no `driver` column exists yet**" |

### 1.5 Hooks pipeline (→ capabilities: **ProgressObservation**, **ToolUseInterception**, **TurnBoundary**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| Hook wiring | `worker_setup.rs:render_settings_json` (≈576–584) | Registers 7 Claude hook types (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, Notification, SessionEnd), each running the `boss-event` shim with `BOSS_RUN_ID` inlined | "A run-tagged stream of lifecycle + tool events" |
| Event shim | `event-shim/src/main.rs` (≈106–256) | Reads Claude hook JSON on stdin, splices `_boss_run_id`, forwards to events socket; on-disk buffer survives engine restart | "Tag every event with run identity; survive engine restart" |
| WorkerEvent enum | `protocol/src/worker_event.rs:14–47` | Variants mirror Claude hook kinds; `StopReason` (≈67) derived by sequencer | "A typed, backend-agnostic event the engine already speaks" |
| Activity machine | `protocol/src/live_worker_state.rs:33–60` + `engine/core/src/live_worker_state.rs:171–241` | Spawning→Idle/Working/WaitingForInput/Terminated, driven by hook kinds | "Drive the activity machine from *any* event source" |
| Editorial PreToolUse | `editorial_hook.rs:evaluate_gh_pretooluse:278` + `app/worker_events.rs:dispatch_editorial_on_pretooluse:418` | Intercepts `gh pr\|issue …`/`cube pr ensure` *before* it runs; Allow / AllowWithRewrite / Block (T946, P576) | "Intercept-and-mutate-or-deny a tool call before it executes" |
| Path / revision guards | `worker_setup.rs` PATH_GUARD_SCRIPT + revision PR guard (≈440–449) | Python inline PreToolUse hook blocks state-dir access / premature PR | "Same interception point, different rule" |
| Completion on Stop | `completion.rs:on_stop_inner:1098–1436` | Stop hook triggers PR detection + nudge/probe routing; staged PR URL cached from PostToolUse | "A 'turn ended' signal that triggers completion + a PR-URL side-channel" |
| Probe injection boundaries | `app/worker_events.rs:dispatch_probe_on_stop:592`, `dispatch_urgent_probe_on_post_tool_use:666` | Non-urgent probes injected on Stop; urgent probes on PostToolUse (`--urgent`) | "Inject text at a turn boundary; pre-empt mid-turn for urgent" |
| Stop-reason inference | `worker_event.rs:normalize_hook_event:84` + sequencer | `Notification`→`Stop` proximity ⇒ `AwaitingInput` | "Distinguish 'idle/done' from 'awaiting human'" |
| Nudge breaker | `nudge_breaker.rs:88–117` | Fingerprints repeated Stop outcomes, trips after 3 | **Driver-agnostic** logic on the event stream |

### 1.6 Transcript reader / format (→ capabilities: **TranscriptAccess**, **StructuredOutput**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| Transcript tail | `engine/transcript-tail/src/lib.rs:TranscriptTail::poll:50` | Newline-delimited JSON, one object/line, tolerant of truncation | "An appendable, tailable run log" |
| Path discovery | `live_status_loop.rs:TranscriptPathCache` (≈352–420) | Path advertised via `transcript_path` field on hook payloads | "Driver announces where its log lives" |
| Redaction walk | `live_status_redact.rs:should_drop_entry:175`, `truncate_large_values:125`, `secret_patterns:319` | Looks for `tool_name`, `tool_input`, `tool_response`, `content[].type=="tool_use"`, `name`, `input` | "Tool calls expose name + input in known fields so secrets/paths can be redacted" |
| Summarizer input shape | `live_status.rs:render_entry:276`, `render_assistant:296` | `type` ∈ user/assistant/system; assistant `content[]` blocks of type text/tool_use/thinking | "A redactable, role-structured view of recent activity" |
| ReviewResult extraction | `pr_review.rs:extract_review_result:215` | Fenced/bare JSON in reviewer's **final assistant message** | "Worker delivers a structured review result" |
| Triage decision | `automation_triage.rs:parse_triage_decision:204` | Plain-text `automation: task <id>` / `automation: skip — …` sentinel in final message | "Worker delivers a structured triage decision" |
| FOLLOWUPS sentinel | `attentions_detector.rs:extract_followups_from_transcript:365` | `FOLLOWUPS:` + fenced JSON array in final message | "Worker delivers structured follow-up suggestions" |
| Final-message read | `completion.rs` (≈2198, ≈2368–2440) | Reads whole transcript, parses markdown, extracts assistant turns | "Read the worker's final structured output" |
| Remote transcript | `remote_transcript.rs:pull_remote_transcript_tail:31` | `tail -c <n> '<path>'` over SSH | "Fetch the run log from a remote host" |
| Remote reattach | `remote_reattach.rs:reattach_remote_runs:56`; `app/worker_events.rs:register_remote_worker_slot:344` | Re-derive live status from hook events after restart; seed slot with model label `"claude"` | "Re-attach to an in-flight run after engine restart" |
| `bossctl agents transcript` | `bossctl/src/main.rs` (≈244) | Renders Claude transcript jsonl | "Expose the run log via RPC" |

**T1414 direction (fold in):** the structured-output cases above (ReviewResult, triage, FOLLOWUPS, PR URL) are moving from *parse-it-out-of-the-transcript* toward *the worker writes it to a known file*. That is strictly more driver-friendly: a file-based structured-output contract needs no transcript-format knowledge at all. This design adopts file-based structured output as the **primary** StructuredOutput mechanism and keeps transcript-sentinel parsing as the Claude driver's *fallback producer*.

### 1.7 Control verbs (→ capability: **ControlVerbs**)

| Finding | Location | Claude assumption | Capability |
|---|---|---|---|
| Probe (inject text) | `app/worker_events.rs:dispatch_probe_on_stop:592` + `protocol/src/engine_app.rs:SendToPane:101` | Text written into the ghostty PTY, read by Claude REPL as next user message | "Inject a user message into a live worker" |
| Interrupt | `protocol/src/engine_app.rs:InterruptWorkerPane:129` | Esc key into the pane cancels Claude's in-flight turn | "Cancel the in-flight turn" |
| Stop / reap (process) | `dead_pid_sweep.rs:probe_pid:329`, `stale_worker_sweep.rs:run_one_pass:104`, `orphan_sweep.rs` | `kill(pid,0)` liveness; pane/process kill; staleness = no hook for 30 min | "Probe liveness; terminate; detect alive-but-wedged" |
| Transient recovery | `transient_error.rs:classify_claude_error:30`, `transient_recovery.rs` | Anthropic error strings/HTTP codes classified transient/permanent; auto-resume via probe | "Classify worker errors; auto-resume retriables" |

The *mechanism* (PTY send-keys, Esc, SIGINT, `kill(pid,0)`) is delivery-specific; the *verbs* (probe / interrupt / stop / reap / classify-error) are the capability. Process liveness and the pane transport are mostly driver-agnostic already; error-string classification is Anthropic-specific.

### 1.8 Direct Anthropic API calls — the engine's own inference (→ capability: **UtilityModel**)

These are **not** the worker driver. They are the engine calling Anthropic's REST API directly for its own background inference. They are the clearest "Boss calls their API directly" cases and a distinct decoupling axis.

| Finding | Location | Model | Purpose |
|---|---|---|---|
| Live-status summarizer | `live_status.rs:52–57,477` | `claude-haiku-4-5-20251001` | One-sentence "what is the worker doing" for kanban |
| Pane summary | `pane_summary.rs:48–53,330` | `claude-sonnet-4-6` | Gerund-phrase pane titlebar |
| Magic wand | `magic_wand.rs:21–26,239` | `claude-sonnet-4-6` | Comment→doc edit, bypasses worker pool |
| Attentions backstop | `attentions_detector.rs:40–42,833` | `claude-haiku-4-5-20251001` | Extract design questions from a transcript |

All four hardcode `https://api.anthropic.com/v1/messages`, `anthropic-version: 2023-06-01`, `x-api-key`, and a pinned model; key resolution via `ANTHROPIC_API_KEY` (with `BOSS_MAGIC_WAND_API_KEY` / `BOSS_BACKSTOP_API_KEY` overrides — `config.rs:159`, `magic_wand.rs:68`, `attentions_detector.rs:61`). P783 (Planner) is documented to reuse the `live_status.rs` substrate.

### 1.9 Prompt composition (→ capability: **PromptComposition**)

| Finding | Location | Claude-flavoured content | Capability |
|---|---|---|---|
| `compose_execution_prompt` | `runner.rs:1056` | Mostly generic; PR-resume block uses `gh pr checkout` / `GIT_DIR=.jj/...`; "print PR URL on final line" assumes the driver's final-message convention | "Compose the worker's first message from work-item context + expected outcome" |
| Editorial-rules block | `runner.rs:render_editorial_rules_block:1393` | Enforcement sentence names "the engine's PreToolUse hook intercepts `gh pr create` …" | "State the editorial contract; name the enforcement the *driver* provides" |
| Effort addendum | `effort.rs:prompt_addendum_for_level:73` | Generic ("write a plan…") | (no coupling) |
| Agent-rules file body | `worker_setup.rs:render_claude_md` (≈150–250) | "Boss-managed worker session… observes this session via **claude hooks**"; `.claude/` gitignored; jj-first VCS; `cube pr ensure`; no-`$EDITOR` | "Inject Boss's worker rules into the driver's instructions file (CLAUDE.md / AGENTS.md / …)" |
| Conflict-resolution fragment | `runner.rs:compose_conflict_resolution_fragment:1828` + `conflict_diagnosis.rs` | jj-based playbook (driver-agnostic prose) | (mostly portable) |

The prompt body is ~90% portable (P284 reached the same conclusion). The Claude-specific residue is: the *name* of the instructions file, the *language* about hooks, and the editorial enforcement sentence — all of which become driver-supplied strings.

### 1.10 MCP / tool definitions

Boss injects **no** MCP servers or tool definitions for Claude today (`worker_setup.rs` writes none; the worker brings its own tool set). This is therefore not a coupling to remove — it is a capability (**ToolProvisioning**) that is *currently unused* and stays unused in v1. Noted so a future driver that *wants* Boss-injected MCP (Copilot's `--additional-mcp-config`) has a named seam.

---

## Alternatives considered

### Alternative A — Generalise P284's spawn-fork to an N-way `enum Backend` (no capability declaration)

Carry a `Backend` enum, `match` on it at the handful of fork points (spawn, hook normaliser, settings render), and add an arm per backend. This is P284's Alternative C scaled from 2 to N.

**Rejected.** It is lowest-common-denominator by construction and fails the operator's first hard requirement. The enum has no way to *declare* that Copilot lacks a PreToolUse interception point or that Codex has a different transcript shape — every divergence becomes a new `match` arm scattered across `completion.rs`, `editorial_hook.rs`, `live_status_loop.rs`, `live_worker_state.rs`. When a backend can't do something, the `match` either silently no-ops (a hidden capability gap) or panics. There is no single place that answers "can this driver do X, and if not, do we degrade or refuse?" The match-arm sprawl is exactly the tax this project exists to avoid, and it grows with every backend × every divergent surface.

### Alternative B — External shim that emulates Claude's surface (`copilot-as-claude`)

Keep the engine 100% Claude-shaped; ship a wrapper per backend that accepts `claude`'s CLI shape, emits `.claude/hooks`-shaped events, and writes a Claude-shaped transcript jsonl. The engine never learns there is more than one backend.

**Rejected** (same reasoning P284 gave for its Alternative B, and it applies more strongly at N backends): backends with no real-time hook surface force the shim to *infer* PreToolUse/PostToolUse/Stop from a log tail and synthesise Claude's JSON — brittle, and failures look like worker bugs rather than backend mismatches. Permission-rule translation (`Bash(:*)` → `--deny-tool 'shell(rm:*)'`) becomes a parser. The principle it violates: incompatibilities should be *visible to the engine*, where the degrade-vs-refuse decision is made — not hidden in a shim that fails low-fidelity.

### Alternative D — Pure data-descriptor drivers (a TOML per backend)

Express each driver as a static descriptor: binary name, flag template, file names, effort table, model menu. No code, just data the engine interpolates.

**Rejected** as the *whole* answer (kept as a *component*). The genuinely divergent surfaces are behavioural, not textual: synthesising a `WorkerEvent` stream from Copilot's JSON, parsing structured output, classifying a backend's error strings, translating a permission policy into a backend's grammar. A data descriptor cannot synthesise an event stream or run a normaliser. The chosen approach *uses* a descriptor for the easy 70% (binary, flags, file names, model menu, effort table) and a trait for the behavioural 30%.

### Alternative C (chosen) — Capabilities-oriented driver trait with a declared capability set and per-capability absence policy

A driver is a trait object that **declares a `CapabilitySet`** and implements the capabilities it claims. Boss queries the declaration, resolves the capabilities a given work-item kind requires, and applies a **per-capability absence policy** (synthesize / degrade / refuse) when the driver lacks one. This satisfies both hard requirements: capabilities are first-class and declared (not lowest-common-denominator), and selection is per-work-item (not global). It absorbs Alternative D's descriptor as the driver's static half.

---

## Chosen approach

### The shape

```
                         ┌───────────────────────────────────────┐
   work item ──────────► │  CapabilityResolver (dispatch gate)    │
   (kind + driver)       │  required(kind) ∩ declared(driver)     │
                         │  → synthesize | degrade | refuse       │
                         └───────────────┬───────────────────────┘
                                         │ resolved plan
                                         ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │  trait AgentDriver  (one trait object per dispatch)               │
   │  • descriptor():   binary, file layout, model menu, effort table  │  ← Alt-D data half
   │  • capabilities(): CapabilitySet (what this driver declares)      │
   │  • spawn / provision / permission / observe / intercept /         │  ← behavioural half
   │    structured-output / control-verbs / transcript                 │
   └──────────────────────────────────────────────────────────────────┘
        ▲ ClaudeDriver (reference)   ▲ CopilotDriver (P284)   ▲ CodexDriver (future)

   Orthogonal axis:  trait UtilityModel  (engine's own background inference)
                     live-status • pane-summary • magic-wand • attentions • Planner
```

Everything downstream of the produced `WorkerEvent` stream and the resolved spawn plan — the dispatch state machine, slot accounting, cube lease, kanban, PR detection, conflict flow — is untouched and stays driver-agnostic (it already is).

### Capabilities

Each capability is a named unit Boss needs. A driver declares, per capability, that it **provides** it (and at what fidelity tier where relevant) or does not. The capability set:

| Capability | What Boss needs | Claude provides | Absence policy (default) |
|---|---|---|---|
| **Spawn** | Build the command/plan that starts a worker against a workspace with a prompt | `claude_invocation` | **Refuse** — a driver that can't spawn is not a driver |
| **WorkspaceProvisioning** | Materialise per-session files (prompt, agent-rules, gitignore); suppress trust prompt | `.claude/*` + `~/.claude.json` pre-trust | **Refuse** (prompt file is mandatory); trust-suppression alone may **degrade** (worker may prompt once) |
| **PermissionPolicy** | Apply Boss's abstract policy (autonomous-honour-denies; reviewer read-only; deny bossctl/state-dir/rm/sudo) | settings.json deny rules + permission-mode | **Refuse** if the driver can't deny the structural set (bossctl/state-dir); **degrade** for finer rules |
| **ModelMenu + EffortMapping** | Resolve effort+override against the driver's model menu; classify family for autonomy default | `effort.rs` tables | **Degrade** — collapse Boss effort onto the driver's coarser knob (e.g. 5→3) |
| **ProgressObservation** | Produce a `WorkerEvent` stream driving the activity machine; fidelity tiers: *rich* (per-tool events) / *coarse* (turn + lifecycle) / *minimal* (process alive only) | rich (hooks) | **Synthesize** from the best channel the driver has; never refuse — minimal tier (alive/exited) always works |
| **ToolUseInterception** | Intercept-and-rewrite-or-deny a tool call before it runs (editorial T946/P576, path guard, revision PR guard) | PreToolUse hooks | **Degrade** to post-hoc (review the artefact after creation, request edit/redaction) **or Refuse** for editorial-required products — operator policy, see open questions |
| **TurnBoundary** | A "turn ended" signal triggering completion detection + probe injection | Stop hook | **Synthesize** (infer from stream idle) or **degrade** to process-exit-only completion |
| **StructuredOutput** | Receive worker's structured results (PR URL, ReviewResult, triage, FOLLOWUPS); **file-based primary** (T1414), transcript-sentinel fallback | file primary + transcript fallback | **Degrade** to file-only (drop sentinel parsing); PR URL via file or `gh` poll always works |
| **TranscriptAccess** | A redactable, role-structured view of the run for summarisation + post-hoc extraction | jsonl transcript | **Degrade** — driver normalises to the canonical field shape, or supplies its own redactor; minimal tier = post-hoc transcript only |
| **ControlVerbs** | probe / interrupt / stop / reap / classify-error | PTY send-keys, Esc, signal, error-string table | **Degrade** per verb — reap (process kill) always works; probe/interrupt may be absent ⇒ the worker is fire-and-forget |
| **ToolProvisioning** (MCP) | Inject MCP servers / tool defs | none today | **Unavailable** — not used in v1 for any driver; not a regression |
| **PromptComposition** | Driver supplies the agent-rules filename, the hook-enforcement wording, and the final-output convention; the body is shared | CLAUDE.md template | **Refuse** if the driver supplies no instructions channel (every real CLI has one) |

`UtilityModel` is the orthogonal capability (§1.8): "run a short background completion for the engine's own inference." It is **not** part of the worker `AgentDriver` — a Copilot work item still gets its live status summarised by whatever utility model the engine is configured with. Default stays Anthropic Haiku/Sonnet; the seam lets an operator point it elsewhere (or at the same provider as the work item's driver) without touching the worker path.

### The absence-policy model

Three dispositions, chosen *per capability per driver*, and resolved at dispatch:

- **Synthesize** — Boss manufactures the signal from a lower-fidelity channel the driver does have. Example: ProgressObservation under Copilot — parse `--output-format json --stream on` into `WorkerEvent`s (this is exactly P284's Q3). The activity machine never knows the difference.
- **Degrade** — Boss runs with reduced fidelity and records that it did. Example: 5-value effort collapsing onto a 3-value knob; coarse live status; post-hoc editorial. Degrade must `log()`/surface what was reduced, so a Copilot row that looks "coarser" is *known*-coarser, not silently broken.
- **Refuse** — Boss will not dispatch this work-item on this driver; it fails **at the dispatch gate, before the pane spawns**, with an actionable message (kanban shows `Errored`, not `Spawning` forever). Example: an editorial-required product dispatched on a driver with no ToolUseInterception, if the operator chose refuse-over-degrade for that surface.

The disposition is a property of `(capability, driver)`, with a per-kind escalation: a work-item *kind* can mark a capability as **required-strict** (force Refuse on absence even if the default is Degrade). This is how "project_design stays Claude-only until validated" (P284 Q6) is expressed generically — `project_design` marks StructuredOutput + ToolUseInterception required-strict, so a driver lacking them is refused for that kind without a bespoke per-kind block.

### Mapping every Part-1 finding onto the interface

| Part-1 finding | Capability | Non-Claude driver satisfies it by… |
|---|---|---|
| 1.1 spawn invocation | Spawn | building its own command line from the resolved plan (Copilot: `copilot -p … --no-ask-user --allow-tool=…`) |
| 1.2 `.claude/` files | WorkspaceProvisioning | its own file layout (`.copilot/AGENTS.md`, `.copilot/initial-prompt.txt`) from the descriptor |
| 1.3 deny rules + mode | PermissionPolicy | translating the abstract deny-set to its grammar (`--deny-tool 'shell(bossctl:*)'`); refuse if it can't deny the structural set |
| 1.4 effort/model tables | ModelMenu + EffortMapping | its own menu (gpt/opus/sonnet) + its own effort collapse (degrade) |
| 1.5 hooks | ProgressObservation / ToolUseInterception / TurnBoundary | synthesize the event stream from JSON-stdout; degrade/refuse interception; synthesize TurnBoundary from stream-idle |
| 1.6 transcript + structured output | TranscriptAccess / StructuredOutput | normalise to the canonical redaction shape; deliver structured output via **file** (T1414) so no transcript parsing is needed |
| 1.7 control verbs | ControlVerbs | map probe/interrupt to its transport (or declare absent ⇒ fire-and-forget); reap via process kill always works |
| 1.8 direct API | UtilityModel | unchanged by worker-driver choice; configurable independently |
| 1.9 prompt | PromptComposition | supply its agent-rules filename + enforcement wording; share the body |
| 1.10 MCP | ToolProvisioning | unused in v1 |

### Genuinely divergent areas — how the model copes

- **Hooks (no analog).** The hardest surface. Resolved by making ProgressObservation a *capability with fidelity tiers* and a **Synthesize** default: Claude provides the rich tier from its hook stream; a hookless driver provides whatever it has (JSON stdout, log tail, or process lifecycle) and Boss synthesises the same `WorkerEvent` stream. ToolUseInterception is the part of "hooks" that *cannot* be synthesised after the fact (you can't un-run a `gh pr create`); that one degrades to post-hoc or refuses.
- **Permissions (different grammar).** PermissionPolicy is an *abstract* policy (a deny-set + an autonomy mode), rendered by the driver. Claude renders settings.json; Copilot renders `--deny-tool` filters; the structural minimum (bossctl, state-dir) is refuse-on-absence so a driver that can't enforce it is never dispatched.
- **Transcript format (different shape).** TranscriptAccess requires the driver to *normalise to* the canonical field names the redactor already knows (`tool_name`/`tool_input`/`tool_response`/`content[].type`) — the field-name compatibility shim lives in the driver, in the one place it matters, so `live_status_redact.rs` is unchanged.
- **Structured output (different mechanism).** Resolved by *moving the primary mechanism off the transcript entirely* (T1414): the worker writes structured output to a known file. Every driver can write a file; only the Claude driver also keeps the transcript-sentinel fallback. This is the single most decoupling-friendly change and should land early.

---

## Mix-and-match & rollout

### How a driver is chosen and recorded

Mirror the existing `model_override` machinery exactly:

- **`tasks.driver TEXT NULL`** — per-row explicit choice (constrained in code to the registered driver names, no SQL `CHECK`, same as `model_override`).
- **`products.default_driver TEXT NULL`** — per-product default.
- Precedence: `task.driver` → `product.default_driver` → engine default (`claude`).
- CLI: `--driver {claude|copilot|codex}` on `boss chore create` / `boss task create` / the `edit` verbs; `--default-driver` on `boss product edit`.

This is the same column P284 calls `backend`. This doc proposes **`driver`** as the canonical name (the abstraction is "driver"; "backend" reads as a global mode). P284, being downstream and not yet implemented, adopts `driver`. (Flagged in open questions in case the operator prefers `backend`.)

### How effort→model resolves per driver

`effort.rs`'s single global table becomes **per-driver**. Each driver's `descriptor()` carries its own model menu, effort→knob table, effort→default-model table, and family classifier. `resolve_spawn_config` keeps its precedence (row → pool → effort-default → product-default → engine-default) but resolves *against the selected driver's menu*:

- **Claude:** today's tables verbatim (5-value effort, sonnet/opus, opus⇒auto-mode).
- **Copilot:** gpt/opus/sonnet menu, 3-value effort (the 5→3 degrade), `--no-ask-user` autonomy.
- **Codex:** its own menu, later.

`model_override` is validated against the *resolved* driver's menu at CLI-parse time: `--driver copilot --model claude-opus-4-7` is rejected with a clear "that's a Claude slug" message (P284's Q1 rule, generalised).

### Migration shape

1. **Claude becomes the reference driver, behaviour-preserving.** The extraction is the bulk of the work and must be byte-for-byte: the spawn line, the `.claude/` files, the deny rules, the hook wiring all move *behind* the trait without changing output. A conformance harness (golden tests) proves equivalence.
2. **P284 implements `CopilotDriver` as a thin addition** on the now-existing interface, declaring its capability set (no ToolUseInterception hooks, coarse ProgressObservation, file-based StructuredOutput, 3-value effort) and riding the absence policies. P284's Q1–Q6 survive as the Copilot driver's answers.
3. **Codex later** as a third driver.

No existing row has a `driver` value; NULL resolves to `claude`; existing dispatches are a no-op. There is no global switch at any point.

---

## Risks / open questions

- **Column name `driver` vs P284's `backend`.** P284 already wrote `tasks.backend` / `products.default_backend` into its design. If P284 ships first, this doc inherits `backend`. Recommend settling on `driver` now (this is upstream) and updating P284's doc. *Operator call — see attentions manifest.*
- **ToolUseInterception absence policy is a security-adjacent choice.** Editorial controls (P576) exist so Boss is safe in environments where leaking internal taxonomy is unacceptable. A driver with no PreToolUse interception can only do *post-hoc* editorial (create the PR, then detect-and-request-edit), which has a window where un-redacted text is live on GitHub. Whether that degrade is acceptable, or whether editorial-required products must **refuse** non-intercepting drivers, is an operator policy decision, not an engineering default. *Operator call.*
- **UtilityModel decoupling: v1 or deferred?** The four direct-Anthropic call sites (§1.8) are independent of the worker driver. Decoupling them behind a `UtilityModel` provider seam is valuable for air-gapped / no-Anthropic-key environments but is *not required* to ship mix-and-match worker drivers. Including it widens v1; deferring it leaves a hardcoded Anthropic dependency in the engine's own inference. *Operator call.*
- **ProgressObservation minimum-fidelity tier.** If a driver gives only process-alive/exited (no per-turn signal), the kanban shows `Working` until exit and the stale-worker sweep (`stale_worker_sweep.rs`, 30-min no-event threshold) may misfire (it keys on hook cadence). The synthesize-from-nothing floor needs a defined behaviour: hold `Working` while alive, and *exempt minimal-tier drivers from the staleness sweep* (or give them a liveness-only sweep). Needs a decision before a minimal-tier driver ships.
- **Stop-reason richness loss.** `AwaitingInput` is derived from `Notification`→`Stop` proximity (Claude-specific). Drivers run autonomous (Copilot `--no-ask-user`) and never emit it; a wedged autonomous worker shows `Working`, not `WaitingForInput`. Acceptable, but the "Working > N min, no events ⇒ flag" heuristic should be the generic replacement.
- **Conformance harness scope.** "Behaviour-preserving Claude extraction" is only credible with golden tests over the spawn line, the rendered settings.json, the CLAUDE.md, and the deny rules. That harness is itself a task and gates the refactor's acceptance.
- **Error classification is Anthropic-specific** (`transient_error.rs`). Transient/permanent recovery currently keys on Anthropic error strings. Each driver needs its own classifier (a small per-driver table) or recovery degrades to "no auto-resume" for non-Claude drivers. Low-risk degrade, but named.
- **Remote/SSH driver-awareness.** `remote_reattach.rs` / `register_remote_worker_slot` seed a slot with the literal label `"claude"`. Remote execution (P545) must carry the driver across the SSH boundary. Out of scope here but a dependency for remote + non-Claude.

## Proposed implementation task breakdown

PR-sized, in dependency order. Effort hints: `trivial | small | medium | large`. "Parallel with" marks same-depth tasks that can run concurrently. Tasks marked *future / not a v1 blocker* are deferred but listed so the graph is complete rather than silently pruned.

**Depth 0 (start immediately; parallel with each other)**

- **Define the driver trait + capability model.** `trait AgentDriver` (descriptor + capabilities + behavioural methods), `CapabilitySet`, `Capability` enum, `AbsenceDisposition { Synthesize, Degrade, Refuse }`, and the per-kind `required-strict` marking. No behaviour yet — types + a `ClaudeDriver` stub that the resolver can hold. **Effort: medium.** Dependencies: none.
- **Add `driver` data model + CLI + resolution.** `tasks.driver` + `products.default_driver` columns (mirror `model_override`/`default_model`), `--driver` / `--default-driver` flags, precedence resolution, and slug-vs-driver cross-validation at parse time. Re-doc the `cli/src/main.rs` "Claude model slug" help text to "model slug for the resolved driver". **Effort: small.** Dependencies: none. *Parallel with the trait task.*

**Depth 1 (after the trait exists)**

- **Extract the Claude Spawn capability.** Move `effort.rs:claude_invocation` + the spawn-line write in `runner.rs` behind `ClaudeDriver::spawn`, behaviour-preserving. **Effort: medium.** Dependencies: Define the driver trait.
- **Extract Claude ProgressObservation.** Make the hook→`WorkerEvent` pipeline (`worker_setup.rs` wiring, `event-shim`, `live_worker_state.rs`) the Claude driver's *rich-tier* producer behind the capability; downstream activity machine untouched. **Effort: large.** Dependencies: Define the driver trait.
- **File-based StructuredOutput contract (fold in T1414).** Define a known-file contract for PR URL / ReviewResult / triage / FOLLOWUPS; make the engine read the file as primary, keep `pr_review.rs` / `automation_triage.rs` / `attentions_detector.rs` transcript-sentinel parsing as the Claude driver's fallback producer. **Effort: large.** Dependencies: Define the driver trait. *Parallel with the two extractions above.*
- **Per-driver ModelMenu + EffortMapping.** Move `effort.rs`'s global tables into the Claude driver's `descriptor()`; `resolve_spawn_config` resolves against the selected driver's menu. **Effort: small.** Dependencies: Add `driver` data model.

**Depth 2 (after the depth-1 extractions they build on)**

- **Extract Claude WorkspaceProvisioning + PromptComposition.** `.claude/` file layout, `CLAUDE.md` template (driver-supplied filename + hook-enforcement wording), `~/.claude.json` pre-trust, all behind the capability. **Effort: medium.** Dependencies: Extract the Claude Spawn capability.
- **Extract Claude PermissionPolicy.** Abstract deny-set + autonomy mode; `ClaudeDriver` renders settings.json deny rules + permission-mode branch; structural minimum (bossctl/state-dir) is refuse-on-absence. **Effort: medium.** Dependencies: Extract the Claude Spawn capability.
- **Extract Claude ToolUseInterception.** Generalise editorial (`editorial_hook.rs`), path guard, and revision PR guard into an interception capability the driver wires; `ClaudeDriver` uses PreToolUse hooks. Define the post-hoc degrade adapter signature (not implemented for any driver yet). **Effort: medium.** Dependencies: Extract Claude ProgressObservation.
- **Extract Claude TranscriptAccess + ControlVerbs.** Driver yields the canonical redactable stream (`ClaudeDriver` normalises its jsonl); probe/interrupt/stop/reap/classify-error behind the capability (`ClaudeDriver` = PTY send-keys / Esc / signal / `transient_error.rs` table). **Effort: medium.** Dependencies: Extract Claude ProgressObservation. *Parallel with the interception extraction.*

**Depth 3 (after the capabilities exist)**

- **CapabilityResolver + dispatch gate.** Given `(kind, driver)`, resolve required ∩ declared, apply absence dispositions, fail-closed at dispatch with an actionable error; encode the per-kind `required-strict` markings (e.g. `project_design` ⇒ StructuredOutput + ToolUseInterception strict). **Effort: medium.** Dependencies: Define the driver trait; Extract the Claude Spawn capability; Extract Claude ProgressObservation.
- **Reference-driver conformance harness.** Golden tests asserting the Claude driver, through the interface, produces the pre-refactor spawn line, settings.json, CLAUDE.md, and deny rules byte-for-byte. Gates acceptance of every extraction. **Effort: medium.** Dependencies: all depth-1/depth-2 extractions.

**Future / not a v1 blocker**

- **`UtilityModel` provider seam** for the four direct-Anthropic call sites (§1.8) + P783 Planner. Decouples the engine's own inference from the worker driver. *future / not a v1 blocker* (operator may pull into v1 — see open questions). **Effort: medium.** Dependencies: none structural.
- **`CopilotDriver` (P284).** The whole of P284, implemented on this interface. *future / not a v1 blocker* — it is the next project, gated on this one. **Effort: large.** Dependencies: CapabilityResolver + dispatch gate.
- **`CodexDriver`.** Third driver. *future / not a v1 blocker.* **Effort: large.** Dependencies: CapabilityResolver + dispatch gate.
- **Per-driver error classifier + minimal-tier staleness handling.** Generic "Working > N min, no events" heuristic; exempt minimal-tier ProgressObservation drivers from the hook-cadence staleness sweep. *future / not a v1 blocker.* **Effort: small.** Dependencies: CapabilityResolver + dispatch gate.
- **Remote/SSH driver-awareness (P545 dependency).** Carry the driver across the SSH boundary; stop seeding remote slots with the literal `"claude"` label. *future / not a v1 blocker.* **Effort: medium.** Dependencies: Add `driver` data model.
- **`ToolProvisioning` (MCP injection) capability.** Unused in v1; named seam for a future driver that wants Boss-injected MCP. *future / not a v1 blocker.* **Effort: medium.** Dependencies: Define the driver trait.
