# Boss: Live Worker Status

## Problem

The Boss UI tells the human three different things about a working agent and none of them answer the question a human actually asks while watching the board:

- `tasks.name` — what the worker was *asked* to do, set when the chore was filed.
- `pane_summaries.summary` — a 3–6-word gerund phrase (e.g. `"fixing the fencer scraper"`) generated **once at task pickup** from the work item's name and description (`engine/src/pane_summary.rs`, PR #185). It never refreshes.
- `live_worker_states.activity` — a coarse `WorkerActivity` enum: `Spawning` / `Working` / `Idle` / `WaitingForInput` / `Errored` / `Terminated`. Drives the kanban Doing dot and the pane status pill (PR #182, PR #193). It tells you the *lifecycle phase*, not the *content of work*.

What the human wants, glancing at the board, is a sentence: *"Investigating why the scroll handler doesn't fire when lane content overflows"* or *"Running tests after the layout fix; one failure in `KanbanLaneViewTests`."* That is neither the original chore name nor a phase label — it is the worker's own moment-to-moment state, in natural language, refreshed often enough to be useful and rarely enough to be cheap.

This doc proposes a **live status** field — a short free-text sentence per slot, alongside (not on top of) `activity` and the existing pane summary — and the engine plumbing, wire shape, UI surfaces, and policy needed to keep it useful, cheap, and safe.

## Goals

- A short, free-text English sentence per active worker, refreshed on a defensible cadence.
- Two render surfaces: Doing-card subtitle and Agents-tab worker header subtitle, fed by the same field.
- Reuse existing engine plumbing (`worker.live_states` topic, `LiveWorkerState`, `transcript_tail`, `pane_summary`'s Anthropic client) wherever possible.
- Per-hour token budget that scales sublinearly with worker count.
- A privacy story strong enough that secrets in transcripts do not surface verbatim on cards.
- A manual off-switch so a focused human can quiet the board.

## Non-Goals

- Replacing `activity` (the coarse enum is still load-bearing for the kanban dot and gating logic — `WorkBoardCardView` keys behaviour on it).
- Replacing the at-pickup `pane_summary` (still the right shape for the pane titlebar's *"Riker is fixing the fencer scraper"* sentence — derived from the work item, stable, and a useful identifier even when the worker is between turns).
- Streaming partial summaries token-by-token — the field updates discretely and the UI just re-renders.
- Building a generic "worker observability" framework. This is one field with one purpose.
- Engine-side decision-making off the field's content (e.g., "if the status mentions the word 'stuck', escalate"). The string is for humans.

## Naming

The new field is referred to throughout as **live status**. `live_worker_states` is already the table/topic name, so `live_worker_states.live_status` is unambiguous. We avoid "summary" because `pane_summaries` is already taken and means something different. We avoid "activity" because `WorkerActivity` is already taken and means something different.

The display string is one sentence, present-continuous voice or short clause; case-sensitive, may include identifiers; aim for ≤ ~120 characters; never multiple lines.

---

## Design Question 1 — Source Signal

### Options

The chore lays out four candidates. Summarised:

| | (a) Probe round-trip | (b) Transcript tail + summarize | (c) Hybrid (b + occasional probe) | (d) Hook-event-only |
|---|---|---|---|---|
| Worker tokens | Yes (model output) | No | Rare | No |
| Engine tokens | Tiny | Yes | Yes | None |
| Authoritativeness | High (worker self-reports) | Medium (depends on transcript content) | Medium-high | Low (events are structural) |
| Latency | Stop-boundary blocked | Independent of worker | Mostly independent | Fastest |
| Secret-leak risk | Low (worker can sanitise) | Real (raw tool output in transcript) | Real | Low |
| Plumbing already exists? | Probe queue exists; no slot for "what are you doing" reply | Yes (`transcript_tail.rs`, `pane_summary`'s API client) | Same as b plus an unused probe slot | Yes (`live_worker_state.apply_event`) |

### Discussion

**(a) Probe** has the appeal of authority — the worker writes its own status. It also has the deepest cost: each refresh is a real Claude turn, billed at worker model rates (Opus 4.7), forcing a Stop-boundary round-trip and ~thousands of input tokens of context per refresh because the worker re-reads its own conversation. At eight concurrent workers and a 30s cadence we'd be looking at ~$/hour numbers in the same ballpark as the workers' actual work, for a feature that is decoration. Probes also serialise behind whatever else the worker is doing, so the "live" status is exactly never live.

**(d) Hooks-only** is free but doesn't tell us anything we don't already render. PreToolUse with `tool_name=Bash` and `tool_input={"command": "cargo test --package boss-engine completion"}` could in principle be templated into *"running cargo test for boss-engine completion"*, but the moment the worker is *thinking* between tools (which is most of the user-visible interesting time) the hook stream goes quiet. We'd render *"using Bash"* — strictly worse than the existing dot.

**(b) Transcript-tail + cheap summarizer** is the right default. We already tail transcripts (`transcript_tail.rs`), already call the Anthropic API for `pane_summary`, and already have a precedent for "fall back silently if the API call fails" (the same module). The summarizer reads only the last N lines of the transcript — assistant text and a redacted tool view — and emits one sentence. Worker is never paused; cost is bounded; fall-back is silent.

**(c) Hybrid** — keep (b) as the steady state, but allow an out-of-band probe trigger from the human (e.g., right-click → "Ask this worker what it's doing"). That gives an escape hatch for the rare *"the summarizer's confused, what's actually happening?"* case without paying for it on every refresh. The probe path uses the existing `pending_probes` queue (`app.rs:295-298`); we'd just need a frontend RPC to seed it with a canned prompt and a `ProbeReplied` event (currently pending per the v2 plan) to land the answer back into `live_status`.

### Recommendation

**Pick (c).** Steady state is (b): tail transcript on Stop boundaries plus a low-rate timer, summarize via Haiku 4.5 (see Q3 below), write the result into `live_worker_states.live_status`, broadcast on the existing `worker.live_states` topic. The probe escape hatch is a nice-to-have that uses existing primitives — ship it in a follow-up, not the first cut.

**Why (b) over (a) for the steady state.** The objection "the transcript may not faithfully reflect what the worker is doing" is real but the transcript is *literally what the worker is doing*. The summarizer is just compressing. A probe would extract the same information through a more expensive channel.

**Why include (c)'s probe at all.** Two cases the transcript can't answer cleanly: a worker that has spent five minutes in a single tool call (no new transcript content), and a worker that's mid-thinking on a contested decision and has produced verbose, contradictory text. In both cases a human asking *"what are you doing"* gets a much cleaner sentence from the worker than from a bag of recent transcript lines.

---

## Design Question 2 — Cadence and Trigger

### Recommendation

**Event-driven primary, time-based secondary, both rate-limited per slot.**

A summarizer call fires for slot S when **any** of:

1. **Stop boundary** for the run — the worker just finished a turn. We have fresh, complete content. (Hook event already lands at `dispatch_live_worker_state` in `app.rs:1446`.)
2. **Tool-call cadence** — every Kth `PostToolUse` for the run, K = 5 by default. Captures long working stretches that don't hit a Stop.
3. **Activity transition** — the moment `activity` flips to `WaitingForInput` or `Errored`, refresh once so the card shows the human-readable reason rather than the stale prior status.
4. **Timer floor** — every 60s if the worker is `Working` and none of (1)–(3) has fired in that window. Catches a slow turn with no tool activity.

A summarizer call **does not fire** when:

- `activity` is `Spawning` (no transcript yet).
- `activity` is `Idle` (status is set to empty; if last status is older than 30s, clear it).
- `activity` is `Terminated` (slot will be released).

### Rate limit

Per-slot floor: at most one summarizer in flight at a time, and at most one completed summary per 15s of wall clock. If a Stop arrives during a 15s cool-down, we coalesce — drop the work, the next event will pick up.

### Why this shape

A pure timer (every 30s, say) wastes calls when the worker is genuinely idle and is too slow when the worker just made a sharp pivot. A pure event drive misses long thinking turns. Combining them is cheap (the rate limit dominates) and gives a UI that *"feels live but isn't twitchy"* — which is the actual ask.

---

## Design Question 3 — Cost / Latency Budget

### Setting a target

Boss-mono workers run on Opus 4.7 at roughly $15/MTok input, $75/MTok output. The existing `pane_summary` module uses Sonnet 4.6 and produces a one-shot 60-token response per spawn. We are about to add a *recurring* summary call. Even at small per-call cost, multiplied by frequency × concurrency, this can balloon.

Conservative estimate, with the recommended cadence:

- 8 workers active.
- Steady-state effective rate (after rate limit + Stop bursts): ~1 summarizer call per slot every 30–45s of *actively working* time. Idle/waiting slots produce zero. Call this 100 calls/hour/slot in the worst case → 800/hour total.
- Per call, we send ~2KB of trimmed transcript context (≈ 500 input tokens after redaction) and expect ≤ 80 output tokens.
- Sonnet 4.6 at $3 / $15 per MTok: ≈ $0.0027 per call → **~$2/hour at 8 workers fully busy**.
- Haiku 4.5 at $1 / $5 per MTok: ≈ $0.0009 per call → **~$0.70/hour at 8 workers fully busy**.

### Recommendation

**Use Haiku 4.5.** A one-sentence summary of recent transcript text is the canonical Haiku-shaped task. Sonnet 4.6 would buy us slightly cleaner phrasing for 3× the price, and the existing one-shot `pane_summary` (which uses Sonnet) is a different problem — that summary is rendered for the run's lifetime, so getting it cleanly is worth more.

**Token budget per call**

- Input: hard cap at 800 tokens after building the prompt. If the transcript window exceeds it, we trim oldest-first.
- Output: `max_tokens = 100`. The prompt asks for ≤ 25 words, so any blow-up gets cut.

**Target $/hour ceiling: $1/hour at 8 fully-busy workers, P95.** This is comfortably below realistic per-worker model spend (Opus 4.7 worker on continuous turns is $20–60/hour). If we go over, we tighten the timer floor first, then truncate the input window further, then drop to event-only.

**Latency**

- API round-trip Haiku 4.5 on 800 input tokens: typically <1s, P99 around 2–3s.
- Worst-case from event to `live_status` updated on the wire: ~3s. Acceptable for a UI label refreshed on a ~30s cadence.

---

## Design Question 4 — Failure and Quiet States

The status is a string and a timestamp. Render rules:

| Worker condition | `live_status` content | UI render |
|---|---|---|
| `Spawning`, no transcript yet | `None` | empty (card shows existing activity dot only) |
| `Working`, first summary still in flight | `None` | empty |
| `Working`, summary landed | the sentence | sentence |
| `Idle` and last summary ≤ 30s old | last sentence (still relevant) | sentence, dimmed |
| `Idle` > 30s | cleared to `None` | empty |
| `WaitingForInput` | last sentence if recent, else `"awaiting input"` | sentence, accent colour to match the "needs human" pill |
| `Errored` | `"errored — check logs"` (literal) | sentence, red |
| `Terminated` | `None` | (slot is being released anyway) |
| Summarizer call failed | keep prior value (do not overwrite) | as before; `last_status_at` timestamp does not advance — the staleness UI takes over after 90s |
| Summarizer call timed out (>5s) | keep prior; log and back off the timer to 120s for this slot for the next two cycles | as before |
| No summarizer activity for >5min while `Working` | dim the rendered status and append `" (stale)"` | dimmed sentence with `(stale)` suffix |

### Why prefer "keep prior" over "blank" on failure

A flickering label that empties on every transient API hiccup is worse than a label that's two minutes old. The pane_summary fallback story is the same — silent failure, retry next time.

### "Awaiting input" literal

When `activity` flips to `WaitingForInput`, the most useful sentence is generally "what was the worker doing right before it hit the prompt?" (which the prior value gives us) — but if there is no prior value, a literal *"awaiting input"* is fine and keeps the card from being misleading. Engine writes this directly without a model call.

---

## Design Question 5 — Storage and Shape

### Recommendation

**`live_status: Option<String>` and `live_status_at: Option<String>` (ISO-8601) added to `LiveWorkerState`**, alongside `activity`. No new table.

Two reasons:

1. The data is per-slot, in-memory, and ephemeral. `LiveWorkerState` is exactly this shape today (slot-keyed, mutex-guarded, snapshot-published — see `engine/src/live_worker_state.rs`).
2. Persisting to disk is unnecessary. On engine restart, every slot is re-`Spawning`d via the startup reconcile path; the live status will be regenerated at the first summarizer trigger. There is no value to surfacing yesterday's status on today's restart.

### Don't reuse `pane_summaries`

Tempting because there's already a model-generated string per work item, but:

- `pane_summaries.summary` is keyed by `work_item_id` and lives across runs.
- `live_status` is keyed by slot and lives only while the run does.
- The two have different cache invalidation rules, different update cadences, different prompts.

Conflating them was the original sin of `activity`-vs-status-pill in the old prototype and we don't want a repeat.

### Optional: snapshot to db on Stop only

If we later want bossctl `agents log` to show *"what was Worf doing 20 minutes ago"*, a thin append-only `live_status_history` table written only on Stop boundaries (one row per Stop) would do it. **Out of scope for v1.** Filed as a follow-up.

### Schema delta

```rust
// protocol/src/live_worker_state.rs
pub struct LiveWorkerState {
    // ... existing fields ...
    pub activity: WorkerActivity,

    /// Free-text one-sentence description of what the worker is
    /// doing right now, generated on the engine side from a tail
    /// of the worker's transcript by a cheap summarizer model.
    /// `None` while a slot is `Spawning` or has never been
    /// summarized; cleared when the worker has been idle long
    /// enough that the prior text is misleading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status: Option<String>,

    /// ISO-8601 timestamp of the most recent successful update
    /// to `live_status`. UI uses this to dim/strike-through stale
    /// values; engine uses it to drive the timer floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status_at: Option<String>,

    // ... existing work_item_* fields ...
}
```

---

## Design Question 6 — Wire Path Engine → UI

### Recommendation

**Reuse the existing `worker.live_states` topic and the `WorkerLiveStatesList` event.** The new fields are additive on `LiveWorkerState`, the topic publisher already broadcasts on every change, and existing subscribers (Agents tab, kanban Doing dot) just see two more fields they can ignore until they want to render them.

The publish call in `engine/src/app.rs:661` is already invoked from `dispatch_live_worker_state` whenever activity transitions. We extend the registry's update API:

```rust
// engine/src/live_worker_state.rs
impl LiveWorkerStateRegistry {
    /// Set the live status string for a slot, stamping the
    /// timestamp. Returns true iff anything changed (so the caller
    /// can decide whether to broadcast).
    pub fn set_live_status(
        &self,
        slot_id: u8,
        status: Option<String>,
    ) -> bool { /* ... */ }
}
```

The new summarizer module (`engine/src/live_status.rs`) calls this, then calls `server_state.broadcast_live_worker_states().await` — same dance as `dispatch_live_worker_state`.

### Why not a new topic

Two costs to a new topic:

1. Subscribers have to be added (macOS app + bossctl).
2. Topic ordering relative to `worker.live_states` becomes a foot-gun — a card might briefly render "Working" + the previous status, or vice versa.

A single topic carrying the full `LiveWorkerState` snapshot keeps the UI's reduce step trivial: replace the slot's local copy.

### Why not push deltas

`WorkerLiveStatesList` already carries the entire snapshot, not deltas. It's at most 8 entries × maybe 1KB each = 8KB per push. Push frequency is low. Delta encoding wouldn't pay for itself.

### Sequence (engine-side update loop and engine→UI push)

```
                   ┌──────────────────────┐
   transcript      │  worker process       │
   .jsonl ─────────│  (claude inside       │
   grows           │   ghostty/tmux pane)  │
                   └─────────┬────────────┘
                             │ writes
              tail polls     ▼
              ┌──────── transcript.jsonl ─────────┐
              │                                    │
       Stop / PostToolUse(K) / timer floor         │
              │                                    │
              ▼                                    │
   ┌──────────────────────────┐                    │
   │ live_status::tick(slot)  │                    │
   │  - read tail (last N)    │                    │
   │  - redact                │                    │
   │  - call Haiku 4.5        │                    │
   │  - set_live_status(...)  │                    │
   └────────────┬─────────────┘                    │
                │ true (changed)                   │
                ▼                                  │
   ┌────────────────────────────────┐              │
   │ broadcast_live_worker_states() │              │
   │   topic = worker.live_states   │              │
   │   payload = full snapshot      │              │
   └────────────┬───────────────────┘              │
                │ FrontendEvent::WorkerLiveStatesList
                ▼                                  │
   ┌──────────────────────────────┐                │
   │ macOS app (subscribers):     │                │
   │  - Agents tab: header subtitle│               │
   │  - Kanban Doing card:        │                │
   │      subtitle under title    │                │
   └──────────────────────────────┘                │
                                                   │
                                                   │
   ┌──────────────────────────┐                    │
   │ events socket (existing) │  hook events ──────┘
   │  PreToolUse / PostToolUse│
   │  Stop / Notification     │
   └─────────┬────────────────┘
             ▼
   apply_event → activity update + change flag
             │
             └─── (already broadcasts; live_status update piggy-backs)
```

The summarizer task lives in a long-running tokio task per running slot, owned by `ServerState`. Stop hook fires `tick(slot)` immediately; PostToolUse counter and timer floor are managed inside the task.

---

## Design Question 7 — Card Render

### Recommendation

`WorkBoardCardView` (`app-macos/Sources/ContentView.swift:907`) already has the structure to take a subtitle row — it currently has a title row (with activity dot, optional crew portrait, task name) and a footer row (project / blocked badges / PR link). Insert the live-status row **between the title row and the footer row**, using:

- `.font(.caption)`
- `.foregroundStyle(.secondary)`
- `.lineLimit(2)`
- `.truncationMode(.tail)`
- only when the card is in the Doing lane and `liveState?.liveStatus` is non-nil/non-empty.

Idle / blank state collapses the row entirely (no awkward empty spacing).

### Why under the title, not in the footer

The footer is for static badges. Live status is the *most informative* thing on the card while a worker is active and deserves to read like prose, not a tag. Two-line truncation under the title puts it where the human's eye lands second.

### Layout pre-existing concerns

The Doing-card layout was just polished in PR #225 (chore `task_18ad60cfc01e7808_10`). The polish was about the activity dot + crew portrait + name being on a single row that doesn't crowd. We don't touch that row. We add a row below it. Card height grows by ~14pt when a status is present; this is acceptable because it's only present in Doing, where cards are already tall enough that the kanban scroll-window absorbs it.

### Mock

```
┌───────────────────────────────────────────────┐
│ ●  [crew] Make boss --json stdout-clean       │
│    investigating why the scroll handler       │
│    doesn't fire when lane content overflows   │
│    [project: Boss]              [PR #185]     │
└───────────────────────────────────────────────┘
```

### Agents-tab worker header subtitle

`WorkersDetailView.slotSubtitle` (`app-macos/Sources/Ghostty/WorkersDetailView.swift:177`) currently reads:

```
"Riker is fixing the fencer scraper"   <-- pane_summary (static)
```

Proposal: when `liveState?.liveStatus` is non-empty, render that *instead*:

```
"running tests after the layout fix; one failure in KanbanLaneViewTests"
```

Fall back to `"Riker is fixing the fencer scraper"` when `liveStatus` is empty (Spawning, idle long enough for clear, or post-failure with no prior). The pane_summary remains a stable identifier-ish thing for tooltips and titles.

Note: live status is a *full sentence*, not a gerund phrase, and rendering it raw (without `"Riker is "` prefix) reads correctly. The two strings have different grammatical shapes on purpose — the static pane_summary is the *role*, the live status is the *moment*.

---

## Design Question 8 — Privacy and Safety

Transcripts contain raw tool output: file contents, bash output, environment variable dumps, fetched HTTP bodies. Anything in the worker's tool path is in the transcript verbatim. A naive summarizer prompt that says *"summarize what's happening"* will quote a file path, an env var, or an API token directly into the status string, where it ends up on the kanban board, which is screenshotted, which is shared.

### Layered defence

**Layer 1 — pre-summarizer redaction.** Before assembling the prompt, the engine runs the transcript window through a redactor that:

- Strips any `tool_input` / `tool_response` JSON values longer than 2KB and replaces with `<truncated>`. (Removes the "we sent the entire file" attack surface.)
- Replaces matches for high-confidence secret patterns with `<redacted>`:
  - Patterns: `sk-[A-Za-z0-9]{20,}`, `ghp_[A-Za-z0-9]{20,}`, `aws_[a-z_]*key[a-z_]*=\S+`, `AKIA[0-9A-Z]{16}`, `Bearer [A-Za-z0-9._-]+`, `password[=:]\s*\S+`, generic 32+ hex, generic 40+ base64.
  - Patterns are deliberately broad. False positives ("a stack trace got `<redacted>` in it") are fine; false negatives are not.
- Drops lines whose `tool_name` is on a deny-list: `Read` of files under `/Users/*/secrets`, anything under `~/.config`, env vars matching common token names.

The redactor lives in `engine/src/live_status_redact.rs` and is testable in isolation. A unit suite must include known secret samples to prove they get redacted.

**Layer 2 — prompt guardrails.** The summarizer prompt explicitly:

- Forbids quoting any literal value longer than 4 words from the input.
- Forbids including any string that looks like a token, key, password, URL, file path under `/private`, `/etc`, `/Users/*/.ssh`, or `~/.config`.
- Asks for description-of-action, not description-of-content. *"Reading the auth config file"* is OK; *"Reading `/Users/.../api_key.txt` containing `sk-ant-…`"* is not.
- Caps at one sentence, ≤25 words.

Even if Layer 1 misses something, a model asked specifically *not to quote literals* is much less likely to surface them in a 25-word output.

**Layer 3 — output filter.** The string returned by the model is run through the same redactor patterns from Layer 1 before being written to `live_status`. If after redaction the string is empty or 90% redaction markers, drop it (set to `None`) and log a warning.

### Privacy posture

This stack is not ironclad. It is "defence in depth, prefer false positive to false negative, and leave a clear log trail when something looks suspicious." It is good enough for an internal tool whose users own the transcripts being summarized; it is **not** good enough to ship to a customer-facing product without further review.

### Worker workspace isolation

Status text never includes the workspace path or other workers' workspace paths. The redactor strips anything under `~/Documents/dev/workspaces/`. Reason: a leaked path means a leaked task name (workspace dir is `mono-agent-NNN` so it doesn't, but `flunge-agent-NNN` is more telling than we'd like).

---

## Design Question 9 — Manual Override and User Control

### Per-worker toggle

Yes. Settings UI in the Agents tab, per slot:

- **Live status: On / Off**. Default: On.
- Off → engine stops calling the summarizer for that slot, clears the field, and the UI falls back to `pane_summary`-derived header subtitle and an empty subtitle row on Doing cards.

Storage: a small `live_status_disabled_slots: HashSet<u8>` on `ServerState`, persisted in the engine's settings KV (the same one that stores other per-slot prefs). Honoured in `live_status::tick`.

### Global cadence override

A single env var on the engine: `BOSS_LIVE_STATUS_TIMER_FLOOR_SEC`, default 60. Used by power users / debugging. Not surfaced in the UI in v1.

### Why this is a small but important feature

Cost — the user might want to silence three of eight workers because the work is sensitive (the transcript will contain customer data, say). Focus — a board with eight live-updating sentences is a lot of motion; a human deeply focused on one card may want the others to stop nudging the eye.

---

## Schema and Wire Summary

### `LiveWorkerState` adds two fields

```rust
pub struct LiveWorkerState {
    // ... existing ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status_at: Option<String>,
}
```

### Topic and event

- Topic: `worker.live_states` (existing).
- Event: `FrontendEvent::WorkerLiveStatesList` (existing). Contains the full updated `Vec<LiveWorkerState>`.

### New engine modules

- `engine/src/live_status.rs` — task lifecycle, trigger fan-in, summarizer client, Anthropic call, retries/backoff. Mirrors `pane_summary.rs` in structure.
- `engine/src/live_status_redact.rs` — the redactor described in Q8. Pure function, no state, fully unit-tested.

### Changed engine modules

- `engine/src/live_worker_state.rs` — add `set_live_status`.
- `engine/src/app.rs` — spawn the per-slot live_status task on slot allocation, stop on release, fan trigger events to it from existing hook dispatch.

### Changed app surfaces

- `app-macos/Sources/Models.swift` — decode the new fields.
- `app-macos/Sources/ContentView.swift` (`WorkBoardCardView`) — render subtitle row.
- `app-macos/Sources/Ghostty/WorkersDetailView.swift` (`slotSubtitle`) — prefer `liveStatus` over `pane_summary`.

### bossctl

- `bossctl agents status` already prints `LiveWorkerState`. New fields appear automatically; consider an output-formatter tweak so the sentence wraps tidily in the table view.

---

## Risks

**R1 — Summarizer drift produces misleading text.** Mitigation: prompt is short and concrete; we use Haiku 4.5 (low creativity tendency on a tightly-scoped task); the field carries `live_status_at` so the UI can dim stale values. Acceptable residual risk: occasional bland or off-target sentence. Worst case: human reads the actual transcript — the new field is decoration on top of existing observability, not a replacement.

**R2 — Cost overrun.** Mitigation: per-slot rate limit, input token cap, Haiku 4.5 default, $/hour budget, off-switch. Worst case: feature flag the whole module behind `BOSS_LIVE_STATUS_ENABLED`, default on, easy to disable globally.

**R3 — Secret leak via status string.** Mitigation: three layers in Q8. Worst case: an internal-only screenshot includes a token. Not catastrophic but embarrassing — hence the layered defence.

**R4 — `live_worker_states` topic broadcast spam.** Each summarizer success is one broadcast on top of the existing activity-driven broadcasts. With 8 workers and a 30s effective rate, that's < 1Hz aggregate. App side already does a full snapshot replace. No regression.

**R5 — Ordering vs `activity`.** A summarizer call could land *just after* the slot transitions to `Idle`, briefly painting a stale sentence under an `Idle` dot. Mitigation: in `set_live_status`, if `activity == Idle && live_status_at < activity_changed_at - 30s`, drop the write.

**R6 — Race between transcript tail and Stop hook.** The transcript file is flushed asynchronously. A Stop hook may fire before the last assistant message lands on disk. Mitigation: `live_status::tick` polls the tail with a 200ms wait + retry on Stop triggers. Falls back to "tick on next event" if still empty.

---

## Follow-up Implementation Chores (to enqueue once approved)

These are bite-sized so each one fits in a single worker session.

1. **Protocol additions**: add `live_status` and `live_status_at` to `LiveWorkerState` in `tools/boss/protocol/src/live_worker_state.rs`. Update Swift mirror in `app-macos/Sources/Models.swift`. Acceptance: serde round-trips; macOS decode tests pass.

2. **Engine: redactor module**: create `engine/src/live_status_redact.rs` with the patterns and the deny-list from Q8. Unit tests on a corpus of synthetic-secret inputs. Acceptance: ≥ 95% redaction on the test corpus, no production calls touch it yet.

3. **Engine: summarizer client + prompt**: create `engine/src/live_status.rs`. Anthropic call, model = Haiku 4.5, prompt with the Q8 guardrails, 5s timeout, silent failure path. Mirror `pane_summary.rs` in shape. Acceptance: integration test with a recorded transcript fixture produces a sane sentence; no transcript content reaches the API verbatim (the redactor is in the path).

4. **Engine: live_status registry update**: add `set_live_status` to `LiveWorkerStateRegistry`. Wire into the existing `broadcast_live_worker_states` path. Acceptance: a unit test mutates `live_status` and observes the change-flag and broadcast.

5. **Engine: trigger fan-in**: spawn a per-slot task on allocation; subscribe to Stop hooks, count PostToolUse, run the timer floor; call `live_status::tick`. Stop on slot release. Acceptance: a recorded session at full Working pace produces ≤ N summaries per minute (per the rate limit).

6. **Engine: per-slot off-switch**: settings KV plumbing for `live_status_disabled_slots`, frontend RPC to set it, honoured in `tick`. Acceptance: toggling off stops summary generation for the slot within one tick interval.

7. **macOS: Doing-card subtitle row**: modify `WorkBoardCardView` to render `liveState?.liveStatus` between the title row and the footer row. Acceptance: snapshot tests covering filled / empty / multi-line / tail-truncated cases.

8. **macOS: Agents-tab header subtitle**: modify `WorkersDetailView.slotSubtitle` to prefer `liveState?.liveStatus`, falling back to `pane_summary` when empty. Acceptance: visual review on a running session; existing pane_summary path still active when status is absent.

9. **macOS: per-worker toggle**: add an "Live status: On/Off" control in the Agents-tab worker row, wired to the engine RPC from chore 6. Acceptance: toggle persists across engine restarts.

10. **Probe escape hatch (optional, hybrid path)**: a frontend RPC that seeds the `pending_probes` queue for a given run with a canned *"In one short sentence, what are you working on?"* and routes the reply (via the not-yet-wired `ProbeReplied` event) into `live_status` directly, bypassing the summarizer for that one update. Out of v1; file as a stretch.

---

## Out of Scope

- Persisting live_status history (`live_status_history` table). File as a separate design once Q8's privacy story is shipped and we know whether anyone actually wants the history.
- Multi-line live status. UI is one-sentence-shaped; if we need more, we already have the full transcript.
- Translating status to non-English (the summarizer prompt is English-locked).
- Fancier UI states (mini-progress bars, ETA estimates). The string is the surface.
