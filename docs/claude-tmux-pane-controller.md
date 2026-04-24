# Claude Tmux Pane Controller

## Overview

This document proposes a Rust library for controlling Claude Code sessions that
run inside `tmux` panes and remain fully attachable by a human user.

The tested control model is:

- a pane runs plain `claude`,
- the user can attach to the same `tmux` session at any time,
- the library sends prompt text via `tmux send-keys`,
- the library reads state via `tmux capture-pane`,
- the library treats Claude as a terminal UI, not as a structured RPC endpoint.

The core design conclusion from live testing is that this is viable as long as
the library is explicit about:

- single-writer turn ownership,
- prompt submission mechanics,
- pane-state polling and debounce,
- environment sanitization on launch,
- user interference and recovery.

## Goals

- Support normal user-facing Claude sessions inside `tmux`.
- Allow a Rust process to launch, inspect, and drive those sessions.
- Detect whether Claude is currently busy without relying on sentinel markers.
- Delay automated input until the pane is idle.
- Relay visible output incrementally to another consumer when needed.
- Preserve user attachability and normal terminal behavior.

## Non-Goals

- Treating Claude Code as a stable machine API.
- Parsing every screen update into a perfect semantic model.
- Supporting concurrent automated writers to one pane.
- Hiding all `tmux` implementation details behind fake abstractions.
- Solving arbitrary terminal automation beyond the Claude use case in v1.

## What We Validated

Live testing against plain `claude` in `tmux` established:

- `tmux send-keys` can reliably inject prompt text.
- `tmux capture-pane -p` returns a readable snapshot of visible pane text.
- `tmux capture-pane -e -p` preserves escapes when a caller needs richer
  terminal state.
- Claude can be controlled while remaining attachable by a user.
- Busy state is visible through the footer text `esc to interrupt`.
- Done state is visible when that footer disappears and the pane stabilizes.
- Partial assistant output is visible during long generations.
- Tool activity is visible through tool blocks such as `Bash(...)`.
- Sending prompt text and submit in one `send-keys` call is not reliable.
- Sending text first and `C-m` in a separate call is reliable.
- Long prompts work when sent in chunks.
- Launch environment matters. `NO_COLOR=1` disables Claude color output.

## Design Principles

### Human Session First

The pane is a real user session first and an automation target second. The
automation layer must not depend on launch flags or display modes that make the
user experience materially worse.

### Single Automated Writer

Only one automated component may inject input into a pane at a time. Human
input is still possible, but the library must serialize its own writes.

### UI-State Detection Over Prompt Tricks

The library should not require sentinel markers to know when a turn is done.
Markers remain useful for higher-level workflows, but pane state must be enough
for base turn control.

### Tmux Is The Integration Boundary

The library should use `tmux` as the stable control surface:

- `new-session`, `new-window`, `split-window`
- `send-keys`
- `capture-pane`
- `display-message`
- `has-session`
- `pipe-pane` when streaming raw bytes is needed

It should not try to inspect Ghostty internals directly.

## Core Concepts

### Manager

The top-level API that owns `tmux` interactions and session discovery.

### Session

A named `tmux` session that contains one or more panes running Claude.

### Pane Controller

A handle for one target pane that can:

- poll state,
- send text,
- submit a turn,
- wait for idle,
- read the latest visible output.

### Turn

One automated prompt submission followed by a wait for Claude to return to an
idle state.

## Proposed Rust API

```rust
pub struct TmuxController {
    // internal command runner and config
}

pub struct LaunchOptions {
    pub session_name: String,
    pub window_name: Option<String>,
    pub command: ClaudeLaunchCommand,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub unset_env: BTreeSet<String>,
}

pub enum ClaudeLaunchCommand {
    PlainClaude,
    Custom(Vec<String>),
}

pub struct PaneHandle {
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
}

pub struct PaneSnapshot {
    pub captured_at: SystemTime,
    pub text: String,
    pub tail: String,
    pub busy: bool,
    pub prompt_visible: bool,
}

pub enum PaneState {
    Starting,
    Idle,
    Busy,
    Unknown,
}

pub struct SendOptions {
    pub chunk_bytes: usize,
    pub inter_chunk_delay: Duration,
}

pub struct WaitOptions {
    pub poll_interval: Duration,
    pub idle_debounce_polls: u32,
    pub timeout: Duration,
}

pub struct TurnResult {
    pub completed_at: SystemTime,
    pub duration: Duration,
    pub final_snapshot: PaneSnapshot,
}

impl TmuxController {
    pub fn new() -> Self;

    pub fn launch_session(&self, options: &LaunchOptions) -> Result<PaneHandle>;

    pub fn find_pane(&self, session_name: &str) -> Result<PaneHandle>;

    pub fn capture(&self, pane: &PaneHandle) -> Result<PaneSnapshot>;

    pub fn state(&self, pane: &PaneHandle) -> Result<PaneState>;

    pub fn send_text(
        &self,
        pane: &PaneHandle,
        text: &str,
        options: &SendOptions,
    ) -> Result<()>;

    pub fn submit(&self, pane: &PaneHandle) -> Result<()>;

    pub fn wait_for_idle(
        &self,
        pane: &PaneHandle,
        options: &WaitOptions,
    ) -> Result<TurnResult>;

    pub fn run_turn(
        &self,
        pane: &PaneHandle,
        prompt: &str,
        send: &SendOptions,
        wait: &WaitOptions,
    ) -> Result<TurnResult>;
}
```

## Recommended Ergonomics Layer

The low-level API above should be wrapped by a higher-level type that enforces
single-writer behavior and exposes a simpler turn contract.

```rust
pub struct ClaudePane {
    controller: TmuxController,
    pane: PaneHandle,
}

impl ClaudePane {
    pub fn wait_until_idle(&self, options: &WaitOptions) -> Result<TurnResult>;

    pub fn say(&self, prompt: &str) -> Result<TurnResult>;

    pub fn snapshot(&self) -> Result<PaneSnapshot>;

    pub fn stream_visible_output(&self) -> Result<impl Iterator<Item = OutputDelta>>;
}
```

`say()` should:

1. wait until the pane is idle,
2. send prompt text in chunks,
3. send a separate `C-m`,
4. wait until the pane is idle again.

## State Detection

### Busy

Treat the pane as busy if the captured text contains:

```text
esc to interrupt
```

This was the most stable live signal across:

- short text replies,
- longer streamed replies,
- tool-using turns.

### Done

Treat the pane as done only when all of the following are true:

1. busy was observed at least once for the turn,
2. `esc to interrupt` is no longer present,
3. the pane tail is unchanged for at least two consecutive polls.

That debounce matters because Claude redraws the footer and lower pane region
while transitioning back to idle.

### Prompt Visibility

The visible `❯` prompt is useful context, but it is not sufficient by itself.
Claude shows the prompt area even while busy. The footer transition is the
stronger signal.

## Output Model

### Primary Read Path

Use:

```text
tmux capture-pane -p
```

This gives a flattened text snapshot of the visible pane and scrollback. It is
the correct source for v1 parsing.

### Optional Escape-Preserving Path

Use:

```text
tmux capture-pane -e -p
```

when the caller wants:

- color-aware debugging,
- OSC hyperlink visibility,
- terminal-style-aware tests.

### Raw Stream Path

Avoid using `pipe-pane` or `script` as the primary parser input. They contain
ANSI cursor movement and redraw noise. They are useful for debugging or replay,
not for base turn detection.

## Prompt Submission Rules

### Do Not Combine Text And Submit In One Call

This proved unreliable:

```text
tmux send-keys -t pane "prompt text" C-m
```

Sometimes the prompt remained in the input box and did not submit.

### Send In Two Phases

Preferred sequence:

1. `send-keys -l` prompt text
2. `send-keys C-m`

### Chunk Long Prompts

Do not send large prompts as a single CLI argument to `tmux send-keys`.
Empirically, a single large call can fail with `command too long`.

Recommended default:

- chunk size: `512` to `1024` bytes
- short inter-chunk delay: `10ms` to `50ms`
- submit in a separate `C-m` call

## Launch And Environment Rules

### Launch Plain Claude

Preferred launch command:

```text
claude
```

The library should not rely on wrappers that materially change the interactive
UI.

### Sanitize Environment

The library should explicitly unset `NO_COLOR` on launch unless the caller opts
in to preserving it.

Recommended policy:

- preserve `TERM`
- preserve `COLORTERM`
- preserve terminal app variables such as `TERM_PROGRAM`
- unset `NO_COLOR`

This is important because `NO_COLOR=1` caused plain Claude inside `tmux` to
render without color during testing.

## Concurrency And User Interference

### Library-Side Locking

Each pane should have a write mutex. Only one library task may send keys to a
pane at a time.

### Human Interference

A user may attach and type during automation. V1 should not try to prevent
this. Instead, it should expose enough state for the caller to detect risk:

- pane changed while no library turn was active,
- pane became busy unexpectedly,
- pane content diverged from the expected pre-send baseline.

Recommended v1 behavior:

- do not try to merge simultaneous writers,
- fail the pending automated turn with a conflict-like error if the pane moved
  in an unexpected direction before send,
- allow callers to retry after a fresh snapshot.

## Suggested Error Model

```rust
pub enum ControllerError {
    TmuxCommandFailed { command: Vec<String>, stderr: String },
    SessionNotFound(String),
    PaneNotFound(String),
    TimedOut { phase: TurnPhase, timeout: Duration },
    BusyNeverObserved,
    UnexpectedPaneMutation,
    Utf8DecodeLossy,
}

pub enum TurnPhase {
    WaitingForIdleBeforeSend,
    WaitingForBusyAfterSubmit,
    WaitingForIdleAfterSubmit,
}
```

`BusyNeverObserved` is useful for very short turns. In that case the caller may
choose to accept an alternative completion condition: pane changed and later
stabilized while idle.

## Suggested Defaults

- `send.chunk_bytes = 900`
- `send.inter_chunk_delay = 30ms`
- `wait.poll_interval = 250ms`
- `wait.idle_debounce_polls = 2`
- `wait.timeout = 2m`

These values matched the live tests well enough to be reasonable defaults.

## Example Turn Flow

```rust
let controller = TmuxController::new();

let pane = controller.find_pane("cc-user")?;

controller.wait_for_idle(&pane, &wait_options)?;
controller.send_text(&pane, "Please say hello.", &send_options)?;
controller.submit(&pane)?;
let result = controller.wait_for_idle(&pane, &wait_options)?;

println!("{}", result.final_snapshot.tail);
```

## Implementation Notes

### Command Runner

Use `std::process::Command` directly for v1. The library needs predictable
stdio, exit status, and argument handling more than it needs a shell.

### Tail Extraction

The state detector should compare only the non-empty tail region rather than
the whole captured pane. Full-pane diffs are too sensitive to scrollback noise.

### Session Discovery

Prefer stable `tmux` targets in the form:

```text
session_name:window_index.pane_index
```

The library may store those fields separately and format them only at call
time.

### Attach UX

The library does not need to embed attach behavior. It only needs to expose the
session name and pane target so another UI can run:

```text
tmux attach -t <session>
```

or a read-only variant.

## Future Work

- A streaming delta API based on snapshot diffing.
- Optional higher-level sentinel support for structured automation tasks.
- A richer parser for Claude tool blocks and assistant message boundaries.
- Pane ownership hints for multi-pane session layouts.
- A libghostty UI layer that embeds the panes while reusing this controller as
  the backend.
