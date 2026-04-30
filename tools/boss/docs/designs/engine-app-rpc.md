# Engine ↔ App pane RPC

Phase 6f needs the engine to drive pane allocation in the macOS app —
when the `ExecutionCoordinator` schedules a run, the engine must
ask the app to host a `libghostty` pane in one of the 8 worker slots,
configure it for the lease, and start `claude` inside it. Today the
engine is a server (clients connect to it); for this we need
**engine→app calls**. This doc picks the transport and shape.

## Why we need it

The engine cannot directly own the libghostty surface — surfaces are
NSViews that live in the AppKit main runloop. Surfaces also own the
pty (and therefore the shell that runs `claude`). So:

- The **app** owns: surface lifetime, pty lifetime, on-screen pane
  layout, slot allocation among the 8 worker panes.
- The **engine** owns: scheduling, lease lifecycle, worker pid
  registry (Phase 6f-1), events socket ingestion (Phase 6f-3), run
  state in SQLite.

The handoff: when a run starts, engine asks app to spawn a worker
pane with a configured workspace + env; app spawns the surface,
extracts the shell pid (parent of the future `claude` process), and
hands the pid back. Engine registers `(shell_pid → run_id)` in the
worker registry so subsequent hook events from the boss-event shim
correlate to the right run via the ancestor walk shipped in 6f-2.

## Constraints

- Reuse existing transport where reasonable. The frontend Unix socket
  already carries `FrontendRequestEnvelope` / `FrontendEventEnvelope`
  between the app and the engine.
- Engine is auto-launched as a child of the app today. The app's pid
  is the engine's parent. We can lean on that for trust.
- Other clients on the frontend socket (the `boss` CLI today, future
  `bossctl`) must not be able to receive engine→app pane requests
  intended for the app.
- Survive an app restart cleanly: any in-flight pane RPCs are
  abandoned with the run; the engine doesn't try to reattach.

## Decision: bidirectional reuse of the frontend socket

Add a new event variant — `FrontendEvent::EngineRequest` — that the
engine pushes only to the **registered app session** on the existing
frontend socket. The app responds via a new request variant
`FrontendRequest::EngineResponse` with a matching `request_id`.

Pros:

- One transport. One auth boundary. No second listener / discovery /
  reconnect logic for a separate app socket.
- The app is already a client of this socket; piggybacking adds a
  message type rather than a connection.
- Engine session machinery already tracks per-session sinks and
  request-id correlation in one direction; extending to a request map
  in the other direction is a few hundred lines.

Cons:

- Conflates "events" with "requests" on the same envelope type. The
  conflation is contained — there's exactly one event variant
  (`EngineRequest`) and one matching response request variant
  (`EngineResponse`).
- The app must respond promptly or the engine call hangs. Mitigation:
  every engine→app call has a timeout, and failure is surfaced as a
  spawn error to the caller (the coordinator).

Rejected alternative: **app-exposes-its-own-socket**. Cleaner
separation but doubles the transport surface (app needs to bind +
accept + auth + reconnect on engine-side), and offers no real benefit
for V2's single-app-instance model.

## Session registration

The app identifies itself once per connection:

```
App  → engine: FrontendRequest::RegisterAppSession {}
Engine → app:  Reply::Ok({ session_id })  (existing reply path)
```

Engine remembers the registered session id. Only one app session is
allowed at a time; a second `RegisterAppSession` call invalidates the
previous registration and re-routes future engine→app traffic to the
new session. Any pending requests on the old session resolve as
`SpawnError::AppDisconnected`.

### Trust check

`RegisterAppSession` is rejected unless the calling session's
`LOCAL_PEERPID` matches the app's pid. The engine knows the app's
pid because the app auto-launched the engine — we use `getppid()` at
engine startup and stash it in `ServerState::app_pid`.

(Phase 7 refines this with the second trust root for `bossctl`.)

## Engine → app: pane RPCs

`EngineToAppRequest` enumerates the calls the engine can make:

```rust
pub enum EngineToAppRequest {
    SpawnWorkerPane(SpawnWorkerPaneInput),
    ReleaseWorkerPane(ReleaseWorkerPaneInput),
}

pub struct SpawnWorkerPaneInput {
    pub run_id: String,
    pub workspace_path: PathBuf,
    pub initial_input: String,    // typically "claude\n"
    pub env: Vec<(String, String)>, // BOSS_LEASE_ID, BOSS_EVENTS_SOCKET, etc.
}

pub struct ReleaseWorkerPaneInput {
    pub slot_id: u8,
    pub kill_grace_seconds: u32, // SIGTERM, then SIGKILL after this
}
```

Responses:

```rust
pub enum EngineToAppResponse {
    SpawnWorkerPane(Result<SpawnWorkerPaneResult, EngineToAppError>),
    ReleaseWorkerPane(Result<(), EngineToAppError>),
}

pub struct SpawnWorkerPaneResult {
    pub slot_id: u8,        // 1..=8
    pub shell_pid: i32,     // pid the engine registers in WorkerRegistry
}

pub enum EngineToAppError {
    NoAvailableSlot,        // all 8 slots in use
    UnknownSlot,            // ReleaseWorkerPane with unknown slot_id
    Internal(String),       // app-side failure detail
}
```

Wire shape:

```
engine → app: FrontendEvent::EngineRequest {
                  request_id: "eng-req-7",
                  request: EngineToAppRequest::SpawnWorkerPane(...)
              }

app → engine: FrontendRequest::EngineResponse {
                  request_id: "eng-req-7",
                  response: EngineToAppResponse::SpawnWorkerPane(Ok(...))
              }
```

Engine maintains a `HashMap<RequestId, oneshot::Sender<Response>>`
gated by the app session lifetime; on app session loss, every pending
sender is dropped with `SpawnError::AppDisconnected`. Calls have a
configurable timeout (default 10s for spawn, 30s for release).

## App-side pane allocation

The app keeps a fixed pool of 8 worker slots. Each slot is either
`Free` or `OccupiedBy(run_id)`. `SpawnWorkerPane` picks the
lowest-numbered free slot, configures a `TerminalPaneSession` with
the supplied workspace path, env, and initial_input, and reports
back. `ReleaseWorkerPane` sends SIGTERM to the surface's shell,
waits for `kill_grace_seconds`, escalates to SIGKILL, then frees
the slot.

`shell_pid` extraction. libghostty's `ghostty_surface_t` does not
yet expose the shell pid directly. Until the upstream API gains it,
the app reads `/proc/self/fd/...` style — actually macOS doesn't
have `/proc`. Realistic options:

1. **Subscribe to `GHOSTTY_ACTION_SHOW_CHILD_EXITED`** to track when
   the shell dies, but that doesn't give us the pid up front.
2. **Use `proc_listpids` filtered by parent = our pid** to find the
   shell after spawn. Brittle (race with multiple panes) but works.
3. **Patch upstream `ghostty` to expose `surface_pty_pid`.** Cleanest;
   tracked separately.

For Phase 6f-5 we'll go with option 2 (post-spawn `proc_listpids`)
and accept the brittleness — the registry's ancestor walk will
forgive misregistration as long as some ancestor of the shim is
correctly registered. Phase 11 includes a follow-up to revisit.

## App restart

If the app process dies, the engine notices when its frontend
session disconnects:

1. All pending engine→app requests resolve as
   `SpawnError::AppDisconnected`.
2. The coordinator handles those errors by transitioning the
   affected runs to `failed_no_pane` (a new wait state, or a
   variant of the existing terminal-failure state — TBD when we
   wire the coordinator).
3. New runs queue but don't spawn until the app re-registers.
4. When the app re-registers, queue drains.

Persistent reattachment of in-flight runs is deferred to Phase 9.

## Engine restart

If the engine restarts (the user kills it for development), the app
loses its connection and reconnects. Re-registration re-establishes
the engine→app channel. Active runs from the previous engine are
already handled by Phase 9's reconcile flow.

## Implementation plan

The 6f work splits into PR-sized slices:

- **6f-4: protocol additions.** Adds `RegisterAppSession`,
  `EngineResponse`, `EngineRequest`, the `EngineToAppRequest` /
  `EngineToAppResponse` enums, and the `EngineToAppError` enum to
  `boss-protocol`. No engine or app implementation. All wire types
  with serde + tests.
- **6f-5: engine-side dispatch.** `ServerState` tracks
  `app_session_id: Option<String>`. New helper
  `engine.send_to_app(request) -> oneshot::Receiver<Response>`.
  Pending-request map keyed by request id. App-disconnect handling.
  Tests against an in-process stub app.
- **6f-6: app-side pane allocator.** Refactor
  `WorkersWorkspaceModel` to support on-demand allocation rather
  than 8 pre-spawned panes. Wire `EngineRequest` handling in
  `ChatViewModel` (or a dedicated coordinator). Implement shell-pid
  lookup via `proc_listpids`.
- **6f-7: spawn-flow wiring.** ExecutionCoordinator calls
  `engine.spawn_worker_pane()` when starting a run. Registers the
  returned `shell_pid` in `WorkerRegistry`. Releases the pane on
  terminal state.
- **6f-8: PoC chat-code cutover.** Remove the chat-style Agents UI
  references in the app. Boss panel in Work mode keeps using
  `ChatViewModel` for now — Phase 7 replaces it with a Boss
  libghostty pane.

Phase 11 (Bazel `GhosttyKit` integration) remains an independent
prereq for shipping the post-cutover Bazel `.app`.

## Open questions

- **Shell-pid timing.** `proc_listpids` is best-effort right after
  spawn — there's a race where the shell hasn't started yet. We may
  need a short retry loop. If it bites, escalate to upstream
  `ghostty` patch.
- **Concurrency on `EngineRequest`.** If the engine fires several
  spawns concurrently (e.g., on coordinator burst), the app needs to
  handle them out of order. The current design's request-id keying
  already handles this; the app implementation must not serialize.
- **`EngineToAppError` variants.** The list above is provisional;
  6f-5 may add more (e.g., `Timeout` for surface init taking too
  long).

## References

- [`work-execution`](work-execution.md) Phase B/C.
- [`v2-design-risks`](v2-design-risks.md) R3 (worker isolation), R5
  (scheduler ownership), R8 (boss-vs-bossctl boundary).
- [Phase 6 plan](../plans/active/v2-implementation.md) — sub-phase
  breakdown.
