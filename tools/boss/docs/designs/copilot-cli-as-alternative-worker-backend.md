# Boss: GitHub Copilot CLI as an Alternative Worker Backend

## Problem

Boss is Claude-only at every layer. The spawn line at `tools/boss/engine/src/runner.rs:279` is literally:

```
claude "$(cat .claude/initial-prompt.txt)"
```

…and that hard-coupling propagates outward through worker setup, hook handling, live-status, transcript ingestion, redaction, permission policy, and prompt composition. The user's day-to-day will increasingly involve GitHub Copilot CLI rather than Claude Code, so Boss needs a second worker backend that can ride alongside Claude without regressing the Claude path.

This document is the design for that second backend. Out of scope: migrating any existing chore off Claude, removing the Claude path, building a generic prompt-composition layer that subsumes both. Claude is and stays the default.

## Goals

- A `backend` field on dispatch-bearing rows (`tasks`) and a `default_backend` on `products`, with a CLI flag (`--backend {claude|copilot}`) and a deterministic precedence rule.
- Engine dispatch forks exactly once, at "spawn the worker," into a backend-specific spawn module. The rest of the engine — dispatch state machine, work taxonomy, cube lease lifecycle, PR detection, kanban, conflict-resolution flow — remains backend-agnostic.
- A per-surface decision (graceful-degrade / unavailable / blocking) for every Boss feature that today assumes a Claude affordance.
- First cut lands so that **one chore** can be created with `--backend copilot`, spawn end-to-end in a leased cube workspace, run to a PR, and surface enough live-status that the kanban does not appear stuck.
- Per-product `default_backend` follows immediately so a whole product can be opted in without per-chore flags. No global cutover.

## Non-Goals

- **Removing the Claude path.** Claude is the default and remains structurally first-class. Any abstraction we introduce must justify itself on the second-backend side.
- **A unified prompt-composition layer that hides which backend you are talking to.** The prompts are 90% backend-agnostic already (workspace path, work item context, expected outcome) and the divergence is in surfaces (CLAUDE.md vs `.github/copilot-instructions.md`, hooks vs no-hooks). A unifying layer is *proposed as a v2 follow-up*; v1 carries two parallel composers.
- **Migrating existing chores.** Existing rows have no `backend` field; they continue to dispatch via the Claude path exactly as today. Migration is a no-op.
- **Re-doing the in-flight effort-and-model work.** [`effort-and-model-estimation`](effort-and-model-estimation.md) lands first under the Claude assumption (`tasks.effort_level`, `tasks.model_override`, `products.default_model`). This project layers on top: it decides how those columns map (or don't) to Copilot. See Q5.
- **Per-stage / mid-task backend swapping.** One worker run uses one backend.
- **A generic "third backend" abstraction.** The design is *prepared* to grow one (the spawn fork is the natural boundary) but does not engineer for hypothetical Cursor / Aider / Cody integrations. Two backends; that's it.

---

## Background — The Copilot CLI Surface

The `copilot` binary ships proprietary (issue tracker at <https://github.com/github/copilot-cli>); the install paths are `npm install -g @github/copilot`, `brew install copilot-cli`, `winget install GitHub.Copilot`, or `curl -fsSL https://gh.io/copilot-install | bash`. Auth requires an active Copilot subscription and a `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` (in that precedence) for non-interactive use. The CLI does **not** ship on the worker host today: `which copilot` returns nothing on the boss workspace machine. Adding it to the worker host is a pre-flight install, not a Boss-build dependency.

The flag surface that matters for Boss (from the public reference at <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference> and <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-programmatic-reference>):

| Concern | Flag | Notes |
|---|---|---|
| Programmatic prompt | `-p` / `--prompt "<text>"` | Equivalent of `claude "$(cat …)"` |
| Suppress stats banner | `-s` / `--silent` | Equivalent of asking Claude for clean output |
| Autonomous (no prompts) | `--no-ask-user` | Disables the agent's `ask_user` tool entirely. *Load-bearing under Boss* — see Q3. |
| Bulk permission grants | `--allow-all` / `--yolo` | "Tools + paths + URLs" all on. |
| Granular tool grants | `--allow-tool='shell(git:*),write'` | Filter syntax: `shell(npm:*)`, `write(.github/copilot-instructions.md)`, `url(github.com)`, `MCP-SERVER(tool)`. |
| Granular tool denies | `--deny-tool=…` | |
| Path access | `--add-dir <path>` (repeatable), `--allow-all-paths` | Default sandbox is `cwd` + tempdir. |
| Model | `--model <slug>` or `COPILOT_MODEL` | Defaults to Claude Sonnet 4.5; supports `claude-sonnet-4`, `gpt-5`, `gpt-5.2`, and `auto`. Slugs are *not* the same shape as `claude --model`. |
| Reasoning | `--effort` / `--reasoning-effort {low\|medium\|high}` | **Three-value range**, not Claude's five. |
| Output format | `--output-format {text\|json}`, `--stream {on\|off}` | The `json` format and `--stream on` together are the only documented machine-readable channel — see Q3. |
| Session id / resume | `--name <n>`, `--resume <id>`, `--continue` | Sessions persist under `$COPILOT_HOME` (`~/.copilot` by default). |
| Logging | `--log-dir <dir>`, `--log-level {none\|error\|warning\|info\|debug\|all\|default}` | The most powerful side-channel for tool-call observability. Format is undocumented. |
| Custom instructions | repo-wide `.github/copilot-instructions.md`; path-specific `.github/instructions/**/*.instructions.md`; `AGENTS.md`; user-level via `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` | No single file analogous to `.claude/CLAUDE.md` — instructions are layered. |
| MCP config | `~/.copilot/mcp-config.json`; `--additional-mcp-config <json\|path>` (per-session); `--disable-builtin-mcps`; `--disable-mcp-server <name>`; `--add-github-mcp-tool` / `--enable-all-github-mcp-tools` | |
| Custom agents | `~/.copilot/agents/`; `--agent <name>` | Roughly analogous to Claude subagents. |
| Subagent depth/fanout | `COPILOT_SUBAGENT_MAX_DEPTH` (default 6), `COPILOT_SUBAGENT_MAX_CONCURRENT` (default 32) | |
| Transcript export | `--share <path>` (markdown), `--share-gist` | **Post-hoc only.** Written when the session ends, not streamed during the run. |
| Auth | `COPILOT_GITHUB_TOKEN` ▸ `GH_TOKEN` ▸ `GITHUB_TOKEN` | |
| Secret redaction | `--secret-env-vars <VAR>` (repeatable) | |
| TTY decorations | `--no-color`, `--no-banner` | |

**What is conspicuously absent:** any external hook system, real-time event socket, structured-event stdout schema, or per-tool-call lifecycle callback. The public docs describe nothing analogous to Claude Code's `PreToolUse` / `PostToolUse` / `Stop` / `Notification` / `SessionStart` / `SessionEnd` hooks. The `--output-format json` + `--stream on` combination is documented to exist, but the schema is not published. `--log-dir` writes log files of an undocumented format.

This single absence is the load-bearing fact in the rest of this design.

---

## Background — Where Boss Assumes Claude Specifically

Drawn from a sweep of the engine source. The point of this section is to fix the surfaces we have to make a per-backend decision about; the resolutions appear in Q1–Q6.

### Hard-coupled

- **Worker spawn.** `runner.rs:279` writes the literal command `claude "$(cat .claude/initial-prompt.txt)"` into the worker pane. The binary name is hardcoded.
- **Hook pipeline → events socket.** `tools/boss/engine/src/worker_setup.rs:227–235` wires all seven Claude hook event types (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `SessionEnd`) to the `boss-event` shim, with `BOSS_RUN_ID` inlined into each command. The engine then consumes them through `tools/boss/protocol/src/worker_event.rs` (`WorkerEvent` enum), and `tools/boss/engine/src/live_worker_state.rs:124–193` translates each hook kind into a worker-activity transition (`Spawning → Idle/Working/WaitingForInput/Terminated`).
- **Worker `settings.json` schema.** `worker_setup.rs:157–237` renders Claude Code's settings shape: `permissions.defaultMode = "auto"`, `permissions.deny: [<rule>…]` in claude-code rule syntax (`Read(…)`, `Edit(…)`, `Bash(:*)`), and the `hooks` map. None of those keys exist for Copilot.
- **CLAUDE.md content.** `worker_setup.rs:62–152` renders a markdown system prompt that hardcodes Claude-Code concepts ("Boss-managed worker session," "claude hooks routed to its events socket"). The free-form *body* is portable; some of the *language* is Claude-specific.

### Incidentally coupled (field names, slug formats)

- **Transcript ingestion.** `transcript_tail.rs` + `live_status_loop.rs:287–300` tail a JSONL file whose path Claude advertises via the `transcript_path` field on hook payloads, and `live_status_redact.rs` walks the JSON looking for `tool_name`, `tool_input`, `tool_response`, `content[].type == "tool_use"`. The *machinery* is generic (tail JSONL, redact secrets, pass to summarizer); the *schema assumptions* are Claude's.
- **Model slug column.** `tasks.model_override` and `products.default_model` are TEXT — the dispatcher doesn't check the slug shape. But the CLI flag's doc comment (`tools/boss/cli/src/main.rs:402`, :629, :705) explicitly says "Claude model slug." A Copilot row with `model_override = "gpt-5.2"` is currently a category error against the column's documented intent.
- **Permission mode.** `worker_setup.rs:224` pins `permissions.defaultMode = "auto"`. The semantics ("autonomous but honor allow/deny rules; never bypass") are universal; the field name and rule syntax are not.
- **Initial-prompt path.** `runner.rs:272` writes the prompt to `.claude/initial-prompt.txt`. The directory name is Claude-flavored. Worker `.claude/.gitignore` (`worker_setup.rs`) similarly assumes the dir.

### Generic / not coupled

- **`WorkerActivity` enum, dispatch state machine, slot accounting, cube lease.** All backend-agnostic. The activity enum is abstract (`Spawning | Idle | Working | WaitingForInput | Errored | Terminated`) — anything that can drive *that* enum can drive Boss's UI.
- **Prompt composition `compose_execution_prompt()` (`runner.rs:340–`).** Generic in structure: work item context + expected outcome + acceptance criterion. Some examples / terminology lean Claude-flavored but nothing requires Claude.
- **PR detection, jj/git plumbing, work taxonomy, GitHub flow.** Already backend-agnostic; the worker is responsible for opening the PR, and the engine just observes the result.
- **Sub-agent handling.** Boss does not inspect Claude's Task tool invocations specifically — they look like any other tool call in the transcript. Copilot's `task` tool would look the same.

---

## Alternatives Considered

### Alternative A — Two parallel engines (forked dispatcher)

Stand up a second engine binary (`boss-engine-copilot`) that mirrors `tools/boss/engine/` with Copilot-specific spawn and hook logic. Coordinator decides which engine handles each work item at dispatch time.

Rejected. Doubles the surface area we have to keep in sync (state DB schema, dispatch events socket, PR detection, conflict resolution, etc.). Every future feature pays a 2× tax. The work taxonomy is the *value* of Boss; forking it to add a second worker shape throws that value away.

### Alternative B — Worker-shim translator

Keep the engine 100% Claude-shaped. Ship a wrapper shell script (`copilot-as-claude`) that pretends to be `claude`: accepts the same CLI shape, emits the same `.claude/hooks` shape, writes a JSONL transcript that matches Claude's. The engine never knows there are two backends.

Rejected. (1) Copilot has no real-time hook surface to translate; the shim would have to *infer* hook events from `--log-dir` or `--output-format json` and synthesise the JSON Boss expects. Brittle. (2) The shim would have to translate model slugs (`opus` → ???), effort levels (Claude's `low|medium|high|xhigh|max` → Copilot's `low|medium|high`), and silently drop features Copilot can't do. Failures look like worker bugs rather than backend mismatches. (3) Permission rule translation (claude-code `Bash(:*)` → copilot `--deny-tool='shell(rm:*)'`) is a parsing nightmare. (4) Users with both `claude` and `copilot` available will set `model_override = "gpt-5.2"` and the shim has to recognize "this slug isn't mine, so route to copilot," which is just *backend selection* with extra steps.

The principle the shim violates: surface incompatibilities should be visible to the engine, where decisions about graceful-degrade vs blocking can be made. Hiding them in a shim makes failures lower-fidelity.

### Alternative C — Dispatcher fork at spawn, parallel hook normalizer, shared everything else (CHOSEN)

The engine carries a `backend: Claude | Copilot` enum. Dispatch decides the backend once, at spawn, and forks:

- `worker_setup::claude::render(...)` vs `worker_setup::copilot::render(...)` produce backend-specific session files.
- `runner::spawn_worker_command()` builds either a `claude …` or `copilot …` line.
- A parallel `protocol::worker_event::normalize_copilot(...)` produces `WorkerEvent`s from whatever observability channel we can wire up under Copilot — `--log-dir` tail, `--output-format json` stdout, or (worst case) coarse polling.
- Everything downstream of `WorkerEvent` (live worker state, kanban, dispatch events, conflict resolution, PR detection) is untouched.

This is the "one branch at the bottom, everything else shared" shape. We pay an abstraction tax exactly twice (worker setup, hook normalizer) and the rest of the engine is unaware.

---

## Chosen Approach

The rest of this doc is Alternative C, broken into the six concrete decisions an implementation worker needs answered.

---

### Q1 — Where does the `claude | copilot` choice live?

#### Options

- **(a)** Per-row only — `tasks.backend TEXT NULL`. Each chore/task chooses at creation; no defaults.
- **(b)** Per-row + per-product default — `tasks.backend` overrides `products.default_backend` overrides hardcoded engine default (`claude`).
- **(c)** Per-row + per-product + engine-wide override (env var or `agent.toml`).
- **(d)** Per-row + per-product + `--effort`-derived (a `large` effort with a Claude-only requirement forces `claude`, etc.).

#### Decision

**(b).** `tasks.backend TEXT NULL` with the same shape as `tasks.model_override` (nullable TEXT, constrained in code as `claude | copilot`, no SQL `CHECK`); `products.default_backend TEXT NULL` mirroring `products.default_model`. CLI flag `--backend {claude|copilot}` on `boss chore create`, `boss task create`, and the chore/task `edit` verbs. Same precedence rule as `model_override`:

1. `task.backend` (per-row explicit).
2. `product.default_backend` (per-product default).
3. Engine default — hardcoded `claude`.

(c) is rejected for v1: an engine-wide override invites surprise (a single env var quietly redirects all dispatches), and we'd have to undo it for the next person who picks up the workspace. If we ever want it, it's a third precedence level we can add later without revisiting the schema. (d) is rejected because effort and backend are orthogonal axes: a `trivial` chore that the user wants Copilot to run is a coherent thing to ask for.

#### Interaction with `effort_level` and `model_override`

The [`effort-and-model-estimation`](effort-and-model-estimation.md) design pins `tasks.effort_level` and `tasks.model_override` to *Claude's* effort vocabulary (`low|medium|high|xhigh|max`) and *Claude's* model slugs. Both columns are TEXT and don't know they're Claude-flavored. Three rules close the gap:

1. **`effort_level` is portable; the dispatcher translates per backend.** The scope-flavored levels (`trivial|small|medium|large|max`) on the DB row are backend-agnostic. The Claude dispatcher already has a level-to-`--effort` table (Q2 of the effort doc — `trivial → low`, `small → medium`, `medium → high`, `large → xhigh`, `max → max`). The Copilot dispatcher carries a parallel table: `trivial → low`, `small/medium → medium`, `large/max → high`. Three-into-five collapses; that is the unavoidable consequence of Copilot's narrower range. Any worker comparing fidelity against Claude should expect coarser tuning under Copilot — explicit graceful-degrade.
2. **`model_override` is backend-scoped at validation time.** When `--model <slug>` is set, the CLI validates the slug against the *resolved* backend (per Q1 precedence). `--backend copilot --model claude-opus-4-7` errors at CLI-parse time with a clear message ("model `claude-opus-4-7` is a Claude slug; Copilot expects e.g. `claude-sonnet-4.6` or `gpt-5.2`"). If `--model` is set without `--backend` and the resolved backend rejects the slug, the dispatcher fails the spawn with the same error wording.
3. **`products.default_model` is paired with `products.default_backend`.** A product that flips `default_backend = copilot` must also clear or reset `default_model` if it still holds a Claude slug; setting only one of the two raises a warning on `boss product edit`. (We don't add a foreign-key constraint — the column is TEXT, not an FK target — but we do refuse-with-warning on the path of least surprise.)

No schema change to `effort_level` or `model_override` columns. No second `copilot_model_override` column. The TEXT field is fine; the discipline lives in the CLI and dispatcher.

---

### Q2 — Worker spawn command shape under Copilot

#### Today (Claude)

```
claude "$(cat .claude/initial-prompt.txt)"
```

…with the worker's `.claude/settings.json` doing the heavy lifting (permissions, hooks).

#### Proposed (Copilot)

```
copilot \
  -p "$(cat .copilot/initial-prompt.txt)" \
  --no-ask-user \
  --add-dir "$WORKSPACE_PATH" \
  --allow-tool "shell(jj:*),shell(git:*),shell(gh:*),shell(bazel:*),shell(cargo:*),shell(npm:*),shell(node:*),shell(ls:*),shell(cat:*),shell(rg:*),shell(grep:*),shell(find:*),shell(test:*),write,read,url(github.com)" \
  --deny-tool "shell(bossctl:*),shell(rm:*),shell(sudo:*)" \
  --secret-env-vars COPILOT_GITHUB_TOKEN \
  --secret-env-vars BOSS_LEASE_ID \
  --model "<resolved slug>" \
  --effort "<resolved level>" \
  --output-format json \
  --stream on \
  --no-banner --no-color \
  --share .copilot/transcript.md
```

A few things to note about that shape.

**`--no-ask-user` is non-negotiable.** Claude has `permissions.defaultMode = "auto"`, which is "run autonomously, honor allow/deny rules." Copilot's analog is `--no-ask-user` (disable the `ask_user` tool entirely) combined with explicit `--allow-tool` / `--deny-tool` filters. Without `--no-ask-user`, a Copilot worker hits the agent's `ask_user` path and hangs in a pane with no notification surface for Boss to observe. **The engine refuses to dispatch a Copilot worker without `--no-ask-user` in the spawn line.** This is structural, not configurable.

**`--allow-tool` does the work `permissions.allow` does under Claude.** The exact tool list above is the *starting set* — equivalent in scope to what Claude's `auto` mode plus our `deny` rules permits today. We don't ship `--yolo` / `--allow-all`: those skip path/URL verification too, which is wider than Claude's `auto` mode, and Boss's deny rules around `bossctl` and the Boss state dir would be silently overridden. The deny list (`bossctl`, `rm`, `sudo`) is the structural minimum; everything else is per-product instructions in `.github/copilot-instructions.md`.

**`--secret-env-vars` substitutes for the inline-env-prefix trick.** Today Boss's hook command is built by inline-prefixing env vars: `BOSS_RUN_ID=… BOSS_EVENTS_SOCKET=… BOSS_LEASE_ID=… boss-event` (`worker_setup.rs:178–185`) because the hook subprocess can't be trusted to inherit them through Claude. Copilot has no hook, so the inline trick has nothing to attach to. Instead we set the env in the worker's pty (already how `lease_id`, `events_socket`, `workspace_path` reach the child today — `spawn_flow.rs:170–235`) and pass `--secret-env-vars` to redact secrets from Copilot's own logs.

**`--output-format json --stream on`.** This is the only documented machine-readable channel from Copilot. Q3 discusses what we can do with it.

**`--share .copilot/transcript.md`.** A post-hoc markdown transcript Boss can re-ingest after the run completes — coarse but real. We do *not* rely on this for live status; we use it as a fallback so a completed Copilot run still leaves the kanban with *something* to render under the "what did the worker do?" history.

**Files renamed.** The worker's per-session directory becomes `.copilot/` (initial-prompt.txt, transcript.md, settings — see Q4), parallel to `.claude/`. The two coexist in the workspace; either can be present, both can if the workspace has done both kinds of work historically. The engine writes only the one matching the backend chosen for the current dispatch.

---

### Q3 — Hook pipeline (the big gap)

This is the surface where Copilot's missing affordances bite hardest. Boss today drives a six-state worker activity machine (`Spawning | Idle | Working | WaitingForInput | Errored | Terminated`) from Claude hook events. Each hook also feeds the live-status summarizer that renders the per-row status line in the kanban. Under Copilot, **none of the seven hook events exist as an external observability channel**.

#### Options for filling the gap

- **(a) `--output-format json --stream on` stdout-tail.** Each agentic turn emits structured JSON to stdout (the public docs confirm `text` and `json` are the two values; they do *not* publish the schema). Boss would tail the worker's stdout, parse each JSON event, and synthesise `WorkerEvent`s. This is the closest analog to Claude's hook stream we can plausibly get from Copilot today.
- **(b) `--log-dir` tail.** Set `--log-level info` (or `debug`) and `--log-dir .copilot/logs/`, then tail the log file. Format is undocumented; we'd have to reverse-engineer it.
- **(c) `--share`-based post-hoc ingestion only.** Read the markdown transcript after the session ends; live status is just "Spawning until Terminated."
- **(d) MCP probe server.** Ship a Boss-controlled MCP server (`boss-probe`) that registers as a tool the agent is mildly nudged to call between phases. The MCP server forwards "I am about to use tool X" / "I have finished using tool X" to Boss's events socket. Highly speculative and requires the agent's cooperation via prompting.
- **(e) Wrapper PTY observer.** Spawn `copilot` under a PTY that the engine controls, scrape ANSI output for tool-call markers. Brittle and feature-gated on Copilot's TUI never changing.

#### Decision

**Primary: (a) `--output-format json --stream on` stdout-tail. Fallback: (c) post-hoc `--share` ingestion. Explicit graceful-degrade on (b)/(d)/(e) as future work.**

Rationale. (a) is the only path the Copilot CLI explicitly documents as machine-readable, and it works *during* the run rather than after. The schema is unpublished but stable across Copilot release notes (the changelog at <https://github.blog/changelog/2025-10-03-github-copilot-cli-enhanced-model-selection-image-support-and-streamlined-ui/> treats `--output-format json` as a public surface), so we treat it as a documented-but-versioned channel: the implementation task spikes the actual schema first, locks a parser to the observed shape, and tracks Copilot's changelog. (c) is the floor: even if (a) returns nothing parseable, `--share` always lands a markdown transcript at session end, which we can ingest into the live-status redactor by translating section headings into synthetic `PreToolUse`/`PostToolUse` pairs.

(b) is a *deferred enrichment*: if the JSON stdout stream omits a kind of event we need (e.g. permission-prompt → Boss's `WaitingForInput`), `--log-dir` tail is the second-best signal. We document it but don't wire it in v1.

(d) and (e) are noted and rejected for v1 — (d) requires prompt-side coordination that's a project of its own, (e) re-introduces a brittleness this design exists to avoid.

#### How `WorkerEvent` is produced from Copilot's JSON stream

The parser lives in `tools/boss/protocol/src/worker_event_copilot.rs` (new file) and exposes `normalize_copilot_event(raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError>`. Same target type as the Claude normalizer (`WorkerEvent` enum at `tools/boss/protocol/src/worker_event.rs:14–47`), same `kind`-tag JSON shape downstream. The mapping table is the design's load-bearing artifact; the implementation task spikes the actual stream shape and fills it in. A *placeholder* mapping (to be revised when the spike completes):

| Copilot event (hypothesised — verify in spike) | → `WorkerEvent` |
|---|---|
| Session start / connect | `SessionStart { source: Startup }` |
| User prompt accepted | `UserPromptSubmit` |
| Tool call started | `PreToolUse { tool_name, tool_input }` |
| Tool call completed | `PostToolUse { tool_name, tool_input, tool_response }` |
| Agent turn end (no follow-up) | `Stop { stop_reason: Completed }` |
| Agent turn end (awaiting `ask_user`) | should never fire — `--no-ask-user` is mandatory. If it does fire, emit `Stop { stop_reason: AwaitingInput }` so the kanban shows `WaitingForInput` and the worker is treated as stuck. |
| Process exit | `SessionEnd { reason }` |

The `Notification` event (`live_worker_state.rs:169–173` — "permission prompt pending, defer until human input") has no direct Copilot analog because `--no-ask-user` is mandatory. We retire it on the Copilot path: there is *never* a `Notification` event because there's nothing for Copilot to notify about. If Copilot stalls (no event for N seconds) the engine's existing synthetic timer (`live_status_loop.rs`, fires every 60s while Working) still produces something; if it stalls *and* the JSON stream produces no events, the kanban shows the row's last-event-at timestamp going stale and the user has the same "is this stuck?" signal they have today.

#### Live-status summarizer

The summarizer (`live_status.rs`) is fed `WorkerEvent`s and transcript-tail output and produces a 1–2 line natural-language status. It is backend-agnostic at the input boundary: it takes the typed event and a redacted transcript snapshot, neither of which knows which CLI produced them. As long as the Copilot normalizer hands the summarizer the same shape, the summarizer works unchanged.

#### Transcript ingestion

`transcript_tail.rs` tails JSONL today. Under Copilot we have two options:

1. **Reuse the stdout-JSON stream as the transcript.** Boss persists the parsed stream to `.copilot/transcript.jsonl` and tails *that*; the on-disk file format matches Claude's enough that the redactor (`live_status_redact.rs`) can keep working. This is the option we take.
2. Use `--share .copilot/transcript.md` only. Markdown — different shape, requires a parallel redactor.

The redactor (`live_status_redact.rs`) looks for `tool_name`, `tool_input`, `tool_response`, and `content[].type == "tool_use"`. The Copilot normalizer is responsible for *writing the persisted JSONL using these same field names*. That is the field-name compatibility shim, in the only place where it matters; no parallel redactor needed.

---

### Q4 — Session files (the `.claude/`-equivalent for Copilot)

Boss writes three files for each Claude worker session: `.claude/initial-prompt.txt`, `.claude/settings.json`, `.claude/CLAUDE.md`, plus a `.claude/.gitignore` that hides them. Under Copilot the parallel set is:

- **`.copilot/initial-prompt.txt`** — the prompt passed via `-p "$(cat …)"`. Same composer as Claude (`compose_execution_prompt()`), with one-line differences in the worker-rules section (described below). Same role: the user message that kicks off the agent.
- **`.copilot/settings.json`** — *empty / not written*. Copilot's `~/.copilot/settings.json` is global, not per-session. Per-session permission and model choices flow through CLI flags (Q2), not a settings file. We don't touch the user's `~/.copilot/settings.json` — that's their machine config.
- **`.github/copilot-instructions.md`** — *not written by Boss.* This is repo-level convention. Boss workers operate in a leased cube workspace that already has the project's `copilot-instructions.md` if one exists. We do *not* inject Boss-flavored worker rules at the repo level — that would persist into the worker's PR and leak into main.
- **`.copilot/AGENTS.md`** — *the new file*. Copilot reads `AGENTS.md` at session start as a per-session agent-rules file. Boss writes Boss's worker rules here (PR requirement, cube lease boundaries, no-`bossctl`, jj-first VCS, no-editor-fallthrough on commit). Content is a direct port of the Claude CLAUDE.md template at `worker_setup.rs:62–152`, with `.copilot/AGENTS.md` substituted for `.claude/CLAUDE.md`, "Claude Code" for "Copilot CLI," and the hook reference dropped. The file is written *inside the workspace*, not in `.github/`, so it doesn't leak into the PR. It is added to `.copilot/.gitignore` (which is itself `*`-gitignored, same trick as `.claude/.gitignore`).
- **`.copilot/.gitignore`** — `*\n`. Hides everything in `.copilot/` from jj/git.
- **`.copilot/transcript.jsonl`** — written by the engine's stdout-tail (Q3), not by Copilot itself. The redactor reads this.

The Claude path is unchanged. Both directories can coexist in a workspace; the engine writes the one matching the current dispatch's backend and nothing else.

#### Permission model translation

Claude's `permissions.deny` rules (claude-code syntax, e.g. `Bash(bossctl:*)`, `Edit(…)`, `Read(<state-dir>/**)`) translate to Copilot `--deny-tool` filters with this table:

| Claude rule | Copilot equivalent |
|---|---|
| `Bash(bossctl:*)` | `--deny-tool 'shell(bossctl:*)'` |
| `Bash(rm:*)` (if present) | `--deny-tool 'shell(rm:*)'` |
| `Read(<state-dir>/**)` | `--deny-tool 'read(<state-dir>/**)'` |
| `Edit(<state-dir>/**)` | `--deny-tool 'write(<state-dir>/**)'` |

The translator lives in `worker_setup::copilot::deny_filters()` and reuses `deny_rules()`'s state-dir derivation logic (`worker_setup.rs:246–`). The translation is one-pass and lossless for the rules we ship today; if a future Claude rule has no Copilot analog, the translator panics at build time rather than silently dropping the rule (failure-loud).

#### Engine-side audit (suspenders)

`worker_sandbox_audit.rs` logs every attempted access to Boss-state paths from a worker, regardless of the worker's deny rules — it's the second line of defence the design doc for sandbox audit calls out. It already operates on filesystem-level path observation, not on Claude's permission system; it works under Copilot unchanged.

---

### Q5 — Per-backend feature gap matrix (the structural call)

For each Boss surface, the v1 disposition:

| Boss surface | Under Claude | Under Copilot | Disposition |
|---|---|---|---|
| Worker spawn | `claude "<prompt>"` | `copilot -p … --no-ask-user --allow-tool=…` | Parallel implementations (`worker_setup::claude`, `worker_setup::copilot`). |
| `PreToolUse` / `PostToolUse` hook | Real-time via shim | Synthesised from `--output-format json` stdout stream | **Graceful degrade** — fidelity depends on Copilot's stream shape, expected to be ≥80% of Claude's PreToolUse/PostToolUse coverage. |
| `SessionStart` / `SessionEnd` hook | Real-time | Synthesised from process start / process exit | **Graceful degrade** — no `source` discrimination (`Startup` only; `Resume`/`Compact` set to `Other`). |
| `Notification` hook (permission-pending) | Real-time | **Not applicable** — `--no-ask-user` is mandatory | **Unavailable** — by design. If a Copilot session emits a permission-prompt event, Boss treats it as a stuck worker and surfaces it as `Errored`. |
| `Stop` hook with stop_reason | `Completed` / `AwaitingInput` / `Interrupted` / `Other` | `Completed` / `Other` (no permission path) | **Graceful degrade** — narrower reason space. |
| Effort level (`trivial..max`) | → Claude `--effort {low..max}` | → Copilot `--effort {low,medium,high}` (3-into-5 collapse) | **Graceful degrade** — see Q1 mapping table. |
| Model override | Claude slug | Copilot slug, validated against backend at CLI parse | **Backend-scoped validation** — invalid slug-for-backend = CLI error. |
| Live-status summarizer | Hook + transcript | Hook (synthesised) + transcript (from JSON stream) | **Generic** — works unchanged. |
| Transcript redaction | Claude JSON field names | Copilot normalizer writes same field names | **Generic** — works unchanged. |
| `worker_sandbox_audit` | Engine-side fs audit | Same | **Generic**. |
| MCP servers | Not configured by Boss today | Not configured by Boss in v1 either (Copilot has `~/.copilot/mcp-config.json` and `--additional-mcp-config`, but Boss doesn't inject anything) | **Unavailable** — not a regression; Boss never wired MCP for Claude either. |
| Sub-agent / Task tool | Opaque to Boss | Same | **Generic**. |
| Worker prompt composition | Claude-flavored "Claude Code" / `.claude/CLAUDE.md` references | Copilot-flavored "Copilot CLI" / `.copilot/AGENTS.md` references; same structure | **Per-backend prompt template; everything else shared**. |
| Cube lease, jj/git, GitHub PR flow, kanban, conflict resolution | All work | All work | **Generic**. |

#### "Blocking" cases (CLI errors at dispatch time)

The engine refuses to dispatch a Copilot worker when *any* of:

- The worker host has no `copilot` binary on PATH. (Same kind of failure as a missing `claude`; surface a clear error in the dispatch attempt log.)
- `--no-ask-user` cannot be set for some reason. (Structural: we always set it; this is just a guard.)
- `model_override` is set to a slug the resolved backend doesn't accept (Claude slug under Copilot or vice-versa).
- An execution kind the design has not validated on Copilot yet — see Q6 — is dispatched as Copilot.

All four errors fail at the dispatcher *before* the worker pane is spawned, so the kanban row shows `Errored` with an actionable message, not `Spawning` forever.

---

### Q6 — Rollout

#### Phase 1 — first chore through end-to-end

The minimum viable cut. One chore can be created with `--backend copilot`, spawn, edit code, push a branch, open a PR. Tests:

1. Schema migration adds `tasks.backend` and `products.default_backend` (no defaults; existing rows are `NULL`).
2. CLI accepts `--backend {claude|copilot}` on `boss chore create` and `boss task create`. Slug-vs-backend cross-validation lives in the CLI parser (rejects `--backend copilot --model claude-opus-4-7` at parse time).
3. Dispatcher resolves backend per Q1 precedence; on `copilot`, builds the spawn command per Q2; on `claude`, runs exactly today's path.
4. `worker_setup::copilot::render(...)` writes `.copilot/initial-prompt.txt` and `.copilot/AGENTS.md` (parallel to today's `.claude/{initial-prompt,settings,CLAUDE}.md`).
5. Stdout-tail spike: implementation task starts by running `copilot -p "say hi" --output-format json --stream on -s` against a throwaway repo, captures and pins the JSON event schema, *then* writes the normalizer in `protocol::worker_event_copilot`. **Spike comes first — design lands without locking the schema.**
6. Live-worker-state machine works (Spawning → Working → … → Terminated) for the chosen Copilot chore. Acceptance bar: the kanban shows the row moving through states, even if PreToolUse/PostToolUse fidelity is lower than Claude's. If the stream gives us *only* SessionStart / SessionEnd we accept that as v1 — `Working` is held while alive, transitions on exit.
7. Restricted execution kinds. Phase 1 is *implementation* kinds only (`task_implementation`, `chore_implementation`). `project_design`, `conflict_resolution`, and any future special kinds remain Claude-only until explicitly validated. This is the "blocking" entry in Q5's matrix: `--backend copilot` with `kind = project_design` errors at dispatch with "Copilot backend not validated for design tasks; use --backend claude or omit."

#### Phase 2 — per-product default

Once Phase 1 has run cleanly for ≥3 distinct chores on at least one product:

- `boss product edit --default-backend copilot` enables per-product defaults.
- The `effort_level` → Copilot `--effort` mapping table is verified against real chore runs (collect chore IDs into an appendix; if `trivial` chores consistently underperform `low` effort under Copilot, bump the mapping).

#### Phase 3 — additional execution kinds

Project design tasks, conflict resolution, anything else. Each kind gets its own validation gate: run on Copilot for a handful of items, verify outcomes are within tolerance of Claude's, then drop the dispatcher's per-kind block.

#### Phase 4 — observability deepening (deferred enrichments)

- `--log-dir` tail to fill PreToolUse/PostToolUse gaps the JSON stream missed.
- MCP probe server for cross-phase events the JSON stream can't see.
- An optional unifying `WorkerBackend` trait in the engine (the v2 abstraction this design defers).

---

### What does NOT change

For reviewers who want the single-page reassurance — none of the following touches this project:

- **Cube workspace lease lifecycle.** Backend-agnostic. `cube workspace lease` writes a workspace, the engine picks a leasee, and the worker child process gets pointed at it. The CLI in the pane doesn't matter.
- **`jj` / `git` / `gh` flow.** Boss workers use `jj` for VCS and `gh` for GitHub, regardless of backend. The Copilot AGENTS.md template will carry the same `jj`-first rule the Claude CLAUDE.md does (`.claude/CLAUDE.md:VCS` section, ported verbatim).
- **PR detection (`PrDetector`, head-sha gating per `boss-engine-pr-auto-bind-headsha-gate`).** Operates on git remotes; backend-agnostic.
- **Kanban (`tools/boss/app-macos/`, `tools/boss/js/`).** Reads `WorkerActivity` and `LiveWorkerState`, both abstract over backend.
- **Work taxonomy.** `WorkItem`, `WorkExecution`, `WorkRun`, all the dispatch state machine. No `backend` references in the taxonomy itself; only the spawn step and the hook normalizer fork.
- **Conflict-resolution flow.** Today's flow at `runner.rs:340` composes a special prompt. It will not move to Copilot in Phase 1 (Q6 restricts to implementation kinds).
- **`worker_sandbox_audit`.** Filesystem-level. Generic.
- **The events socket and dispatch-events pipeline.** Speaks `WorkerEvent` and `WorkerActivity`; neither knows what CLI was on the other end of the hook.

The biggest single "secretly Claude-specific" wrinkle this design surfaces is the `model_override` column's *meaning*. Today it documents "Claude model slug" in the CLI help text. We fix that with a one-line edit in `cli/src/main.rs:402, 629, 705` — re-doc to "model slug for the resolved backend" — and let the validator do the heavy lifting.

---

## Risks and Open Questions

- **The `--output-format json --stream on` schema is undocumented.** This is the single biggest dependency in the design. The implementation must spike the schema before locking the normalizer. If the schema turns out to lack PreToolUse / PostToolUse boundaries, Phase 1's live-status fidelity drops to "Working until Terminated, plus a post-hoc transcript." We can ship that — the kanban will look coarser for Copilot rows than for Claude rows — but it's worth knowing on the way in.
- **Copilot CLI release cadence.** Copilot ships frequently; the JSON schema *might* drift. The spike must capture the version (`copilot --version`) and the spec must include a "pinning policy" — if a Copilot upgrade breaks the normalizer, the dispatcher should fail closed (refuse to dispatch Copilot workers and surface an actionable error) rather than silently produce garbage `WorkerEvent`s.
- **What happens when `copilot` is not installed on the worker host?** Phase 1 should detect this at dispatch time and fail with a one-line install hint. Open: do we automate the install (`brew install copilot-cli`) from the engine, or refuse and let the human install? Recommend the latter for v1 — engine-driven installs of proprietary tools are a separate trust boundary.
- **Token / quota accounting.** Boss has none today for Claude. Copilot has its own quota model tied to the Copilot subscription. v1 doesn't add cost accounting on either side. Surface this in the design doc as a known absence so it doesn't get conflated with a Copilot-specific blocker.
- **MCP injection.** Boss doesn't inject MCP servers today for Claude either. The Copilot CLI's MCP surface is *richer* than what we use under Claude (`--additional-mcp-config` lets you per-session-add a JSON config); a future Boss could push project-specific MCPs to the worker. Not v1.
- **`AGENTS.md` collision.** Some repos may already have an `AGENTS.md` at the workspace root for human authors. Boss's `.copilot/AGENTS.md` is inside `.copilot/` so it shouldn't collide, but Copilot's spec for which `AGENTS.md` it reads (workspace root vs cwd vs both) is not pinned in the public docs. The spike should test this and the doc should be updated with the answer before Phase 1 ships.
- **Effort mapping coarseness.** Three-into-five (Claude's `low|medium|high|xhigh|max` ↔ Copilot's `low|medium|high`) means `medium` and `large` collapse onto Copilot `high`, and `xhigh` has no native target. Phase 2 verifies whether this matters in practice; if `large`-effort chores routinely underperform on Copilot `high`, we have no headroom to push higher. This is a structural Copilot constraint, not something Boss can fix.
- **`Notification` retirement.** Removing the `Notification` activity-transition path under Copilot means a stalled Copilot worker shows as `Working` (no events) rather than `WaitingForInput`. Acceptable for v1; if it becomes confusing, add a "Working > 5min with no events" heuristic to bump to `WaitingForInput`.
- **Conflict resolution under Copilot.** Q6's Phase 1 restriction leaves conflict resolution Claude-only. The conflict-resolution prompt (`runner.rs:491–550+`) embeds a templated diagnosis from `conflict_diagnosis.rs`. The prompt is portable; what's untested is whether Copilot will run the multi-step playbook the same way. Phase 3 question.
- **Custom agents.** Copilot's `--agent <name>` could be a more powerful integration point than Boss currently uses on the Claude side (Claude doesn't have a direct agent flag — Boss uses the Task tool). Not v1, but the option exists.

---

## Follow-up implementation tasks

For the coordinator / user to file as separate Boss tasks. Listed here, **not created from this worker**.

1. **Schema + CLI flag** — add `tasks.backend` and `products.default_backend` columns (TEXT NULL, mirroring `model_override` / `default_model`); add `--backend {claude|copilot}` to `boss chore create`, `boss task create`, and chore/task `edit`; add `--default-backend` to `boss product edit`. Validation: reject Claude slugs under Copilot and vice-versa at parse time. *Schema-only; no spawn changes.*
2. **JSON stream schema spike** — run `copilot -p "create hello.txt with content 'hi'" --output-format json --stream on -s --no-ask-user --allow-tool=write --add-dir .` in a throwaway repo, capture the event stream, document the schema in an appendix to *this* design doc, pin the Copilot version. *Investigation; output is documentation, not code.*
3. **Copilot normalizer** — `protocol::worker_event_copilot::normalize_copilot_event()`, returning `WorkerEvent`. Schema from task 2. Persists parsed events to `.copilot/transcript.jsonl` so the redactor can ingest them. *Code; gated on task 2.*
4. **Per-backend `worker_setup`** — split `worker_setup` into `worker_setup::claude` (current behavior, renamed) and `worker_setup::copilot` (writes `.copilot/initial-prompt.txt`, `.copilot/AGENTS.md`, `.copilot/.gitignore`; translates deny rules to `--deny-tool` filters; does *not* write a settings.json). *Code refactor + new module.*
5. **Dispatcher fork** — `runner.rs:264–279` calls into a backend-resolved spawn module. Existing spawn flow becomes the `Claude` arm. New `Copilot` arm builds the command per Q2. *Code change at the single bottleneck.*
6. **Stdout-tail driver** — engine-side task that reads the Copilot worker's stdout, parses with the normalizer, ships events to the events socket and persists JSONL. Parallels the role of the `boss-event` shim under Claude, but engine-driven instead of subprocess-driven. *New engine component.*
7. **Effort/model mapping table** — Copilot dispatcher's `--effort` and `--model` resolution. Mirror table from Q1. *Small code change.*
8. **Phase 1 acceptance run** — file one Copilot-backed chore, run it to a PR, verify kanban states transition, capture observations. *Test / validation pass.*
9. **Per-product default backend** — Phase 2; gated on Phase 1 stable. *Schema column already exists from task 1; this just enables the CLI surface on `boss product`.*
10. **Execution-kind expansion** — Phase 3; drop the dispatcher's `kind == project_design || kind == conflict_resolution` block one at a time, with a validation chore for each. *Per-kind acceptance work.*
