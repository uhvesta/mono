# boss-event

`boss-event` is a tiny standalone binary that forwards claude-hook
payloads to the Boss engine. The engine configures each worker's claude
hooks (via its per-worker settings-file template) to spawn this shim
with the hook payload arriving on stdin; the shim reads stdin to EOF,
delivers the bytes to the engine's Unix-domain events socket, and exits.
It exists to keep the worker's hot path decoupled from the engine: the
hook fires, hands off its JSON, and never blocks on engine availability.

## How it fits

The shim is a leaf: it has no internal crate dependencies and nothing
depends on it as a library — it is reached only at worker runtime, when
claude execs it per hook event. Its sole contract with the rest of Boss
is the raw hook JSON written to the socket named by `BOSS_EVENTS_SOCKET`.
The engine derives the worker's lease from the connecting peer's PID, so
the shim embeds no lease identity. When `BOSS_RUN_ID` is set it splices
that id into the JSON object so the engine can correlate the event to a
run without a shell-pid lookup; on any failure (env unset, payload not a
JSON object) it forwards the original bytes unchanged.

## Resilience

The engine is allowed to restart out from under a running worker, so the
shim is built to survive an unreachable engine without losing events
or stalling the worker. Delivery uses bounded connect-retry with
exponential backoff (a wall-time budget of roughly ten seconds), plus a
single mid-send reconnect if a good connection breaks before the payload
is fully written.

When delivery still fails, the event is appended to a per-workspace
on-disk buffer (a `.jsonl` file under the workspace's `.boss/`
directory) and the shim exits successfully, letting the worker keep
moving. On its next successful run the shim opportunistically drains
that buffer oldest-first before sending the current event, preserving
FIFO order on the engine's accept queue; draining stops at the first
failure so a still-down engine leaves the remainder queued. The buffer
is capped at a fixed number of most-recent events so a persistently-down
engine cannot grow it without bound, and concurrent shim invocations
coordinate through an advisory file lock.

The implementation is deliberately small and synchronous — local file
I/O and retry, no async runtime. The retry schedule and workspace/buffer
locations are configurable through environment variables (primarily for
tests, which override the backoff to keep wall time bounded).
