# Boss V2 — Design Risks

## Purpose

The Boss V2 plan ([swiftui-boss-v2.md](../plans/active/swiftui-boss-v2.md))
introduces several large architectural decisions at once. Some of those
decisions are easy to reverse later; others lock in months of work and a
specific failure surface.

This document tracks the load-bearing decisions — the ones where picking wrong
is expensive — and gives each one a small, targeted exploration that resolves
the unknowns before we commit.

The intended workflow is:

1. List the risk and why it matters.
2. List the candidate options.
3. Identify hard constraints that disqualify options outright.
4. Identify the decisive unknowns — what we don't know that would tip the
   decision.
5. Define the smallest POC or investigation that resolves those unknowns.
6. Record the decision and its rationale.

We work through risks one at a time. The plan in `swiftui-boss-v2.md` should
be revised once the major risks have decisions.

## Risk template

```
## RN: <short title>

### Why it matters
<what becomes hard to reverse if we choose wrong>

### Options
<candidate paths>

### Hard constraints
<things that disqualify an option outright; "no" here ends the conversation
for that option>

### Decision dimensions
<axes we trade off when no option is disqualified>

### Decisive unknowns
<what we don't currently know that would tip the choice>

### Proposed exploration
<smallest experiment, conversation, or document review that resolves the
unknowns>

### Resolution criteria
<how we know we have an answer>

### Decision
<filled in last>
```

## R1: Worker shape — terminal-embed vs native chat

### Why it matters

Every other V2 decision sits on top of this one:

- the control surface (`bossctl`, probing, capacity)
- worker isolation
- per-worker workspaces
- crash/resume behavior
- the ongoing rendering surface we own forever

Picking "native chat via SDK/ACP" and then reversing means scrapping a custom
client and a large rendering layer. Picking "terminal-embed via libghostty"
and reversing means swapping pane contents — much cheaper.

The decision is also the one most exposed to external constraints (company
policy, upstream API stability) that we do not fully control.

### Options

1. **Terminal-embed.** Each worker is an unmodified `claude` session running
   inside an embedded `libghostty` surface. Boss is also a `claude` session in
   its own pane. We inherit Claude Code's entire product surface (slash
   commands, tool rendering, plan mode, hooks, todos, MCP UX, paste, image
   attach, `/memory`, etc.). Control is via keystroke injection plus an
   out-of-band channel (hooks, session JSONL, or screen scrape).

2. **Native chat via SDK/ACP.** Each worker is a Claude session driven by our
   own client (Claude SDK or an ACP client we own). We render messages and
   tool calls in SwiftUI. Control is via structured API calls; observation is
   via structured events.

3. **Hybrid.** Terminal-embed for fidelity, with a structured side-channel
   (e.g. Claude Code hooks) to give Boss orchestration-grade signal without
   scraping the screen.

### Hard constraints

- **Company tool/agent-client policy.** ACP and SDK-based workers require us
  to run a custom Claude client that implements its own tool handlers (file
  edit, bash, MCP, etc.). If the user's company restricts custom Claude
  clients to the official `claude` binary, options 2 and 3-via-SDK are
  disqualified outright.
- **`libghostty` build viability.** Embedding requires a private C API
  (`ghostty_surface_new`), Zig 0.15 toolchain, and the macOS Metal toolchain.
  The existing prototype at `tools/ghostty-proto/` proves this is buildable
  today, but upstream churn could break us. This is a soft constraint, not a
  hard one — it is a maintenance cost, not a disqualifier.
- **Claude Code stability.** Terminal-embed inherits whatever Claude Code
  ships, including breaking UX changes. Hooks are a documented extension
  point; screen output is not.

### Decision dimensions

| Axis | Terminal-embed | Native chat (SDK/ACP) |
|---|---|---|
| Rendering surface inherited | Everything Claude Code ships, free | Almost nothing; we reimplement each tool's UI |
| Control fidelity | Keystroke injection; observation via hooks/scrape/JSONL | Structured events both directions |
| Failure recovery | `claude --resume` exists; reattach a terminal | We own resume entirely |
| Boss-side fidelity | Strong — Boss is a real Claude TUI | Weak unless we build a lot |
| Worker-side fidelity | Strong, but workers are dense TUI panes | Can be nicer if we build the chrome |
| Company policy fit | Runs the approved binary unchanged | Requires custom-client approval |
| Reversibility | Easy to swap pane contents to chat later | Hard — humans grow to depend on chat affordances |
| POC cost | Existing 5-pane prototype to build on | None yet; substantial upfront |

### Decisive unknowns

1. **Policy.** Does the user's company allow a custom Claude client (SDK or
   ACP) running its own tool handlers, or only the official `claude` binary?
   This is a yes/no question with a single answer; until it's answered, we
   cannot evaluate option 2 honestly.

2. **Hook coverage.** Do Claude Code's hooks (`PreToolUse`, `PostToolUse`,
   `Stop`, `UserPromptSubmit`, etc.) carry enough signal for Boss-side
   orchestration — at minimum: turn-started, turn-finished, tool-invoked,
   blocked-on-prompt, exited? If yes, terminal-embed gets structured events
   without sacrificing TUI fidelity.

3. **Session JSONL viability.** Claude Code writes session transcripts to
   disk. Are they stable enough in format and timing to use as a secondary
   structured channel for richer observation (e.g. extracting probe
   responses)? This is a fallback if hooks under-cover.

4. **Boss product gap.** What specifically does Boss want to give the human
   that Claude Code's TUI doesn't already give? If most of the wanted
   affordances are *chrome around* a terminal (work-item context next to the
   pane, status chips, fixed grid layout, focus management, keyboard
   shortcuts to switch panes), terminal-embed is sufficient. If the wanted
   affordances are *inside* the conversation (collapsible tool calls,
   click-to-jump-to-file, native diff viewer, native attachment UX), that
   pushes toward native chat.

### Proposed exploration

Done in this order, stopping early if a hard constraint resolves the
question:

1. **Resolve policy.** A short conversation or document check answers
   unknown 1. Output: written note in this doc — "custom clients allowed
   under conditions X" or "official binary only."

2. **Hook coverage POC.** Extend the existing `tools/ghostty-proto/`
   prototype with a Claude Code hook configuration that writes
   `PreToolUse` / `PostToolUse` / `Stop` / `UserPromptSubmit` events to a
   Unix domain socket. Have a small listener log them. Run a typical task
   in the embedded pane and verify the event stream is sufficient to
   answer:

   - is the worker currently in a turn?
   - did the worker stop, and why?
   - is the worker awaiting input?
   - did the worker invoke a destructive tool?

   Output: a written assessment in this doc with the event log and any
   gaps.

3. **Session JSONL inspection.** Read the JSONL files Claude Code writes
   for the same task. Note format stability, timing, and whether they
   contain anything hooks miss (e.g. the assistant's last message text for
   probe-style queries). Output: a paragraph in this doc.

4. **Boss product gap inventory.** List 5–10 concrete affordances we want
   in Boss V2 that go beyond what `claude` already shows. For each, mark
   "chrome around the pane" vs "inside the conversation." Output: a short
   table in this doc.

5. **Decide.** With 1–4 in hand, write the decision and rationale below.

### Resolution criteria

We have an answer when:

- the policy question has a definitive yes/no
- the hook + JSONL channels have been characterized concretely (not
  speculatively)
- the Boss product-gap inventory makes it clear whether the wanted
  affordances are chrome-around or inside-the-conversation
- a written decision can point at those facts as its rationale

### Decision

_Pending exploration._

## Risk backlog

These risks have been identified but not yet worked through. They are
listed here so we don't lose them; we'll write each one up properly when
we get to it. Order is rough priority, not strict sequence.

- **R2: Worker → Boss structured channel.** Even with R1 resolved, we need
  to pick the concrete mechanism (hooks, JSONL, SDK events, screen
  scrape, or a layered combination) and define what events Boss
  subscribes to. Closely linked to R1.
- **R3: Worker isolation model.** How do we stop a worker from invoking
  Boss-only authority (e.g. `bossctl`) or otherwise escalating? PATH
  separation, auth tokens on the control socket, sandboxing, or
  combinations.
- **R4: Per-worker workspace.** Eight concurrent Claude sessions cannot
  share one repo checkout. Decide between git worktrees, fresh clones,
  or a workspace pool, and link the choice to the work-item lifecycle.
- **R5: Scheduler ownership.** Boss-Claude and the human can both start
  work. Decide which component arbitrates capacity and assignment, and
  what intent API both go through.
- **R6: Crash and resume.** What persists across app restarts? How do we
  reattach to running `claude` sessions, and which state lives in the
  engine vs the app vs Claude Code itself?
- **R7: Review and approval flow.** "Ready for review" is a state in the
  plan but the human's review affordance is undefined. Decide where in
  Work mode this lives.
- **R8: `boss` vs `bossctl` boundary.** `work start` straddles durable
  state and live orchestration. Decide whether they're two CLI personas
  on one backend, or genuinely separate services.
