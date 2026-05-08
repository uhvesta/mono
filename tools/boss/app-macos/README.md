# Boss macOS App (PoC)

SwiftUI frontend for the boss PoC.

## One-command launcher

From repo root:

```bash
export ANTHROPIC_API_KEY=...
tools/boss/scripts/run-macos-poc.sh
```

Engine logs are written to `/tmp/boss-engine.log` by default (override with
`BOSS_ENGINE_LOG_PATH`).
Engine PID is written to `/tmp/boss-engine.pid` by default (override with
`BOSS_ENGINE_PID_PATH`).
Engine lifecycle events (start, every socket bind, shutdown — clean,
signalled, or panic) are appended as JSON lines to
`~/Library/Application Support/Boss/engine-audit.log` (override with
`BOSS_ENGINE_AUDIT_PATH`). The file lives outside `state.db` so it
survives db wipes; the engine rotates it in-place when it grows past
2 MiB. See [Forensic / audit log](#forensic--audit-log) below.
Internal system status messages are hidden by default. Set
`BOSS_SHOW_SYSTEM_MESSAGES=1` to show them in the chat transcript.

## Default flow (auto-launch engine)

Run the app and let it launch the engine automatically:

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/app-macos:Boss
```

By default the app launches:

```bash
bazel run //tools/boss/engine:engine -- --mode=server --socket-path /tmp/boss-engine.sock
```

By default, the engine uses:

```bash
npx -y @zed-industries/claude-code-acp@0.16.1
```

The engine runs from the workspace root and prefers `NVM_BIN` on `PATH` when
spawning the ACP adapter.

When auto-start is enabled, the app will:

- reuse an existing engine process from the PID file when available,
- otherwise launch a new engine,
- keep the engine running when the app exits (unless `BOSS_ENGINE_STOP_ON_EXIT=1`).

## External engine mode

Disable auto-start and point the app to an existing socket:

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/engine:engine -- --mode=server --socket-path /tmp/boss-engine.sock
```

```bash
BOSS_ENGINE_AUTOSTART=0 BOSS_SOCKET_PATH=/tmp/boss-engine.sock bazel run //tools/boss/app-macos:Boss
```

## Overrides

- `BOSS_SOCKET_PATH`: unix socket path (default `/tmp/boss-engine.sock`)
- `BOSS_ENGINE_AUTOSTART`: set `0` to disable app-managed engine launch
- `BOSS_ENGINE_CMD`: custom command used when auto-start is enabled
- `BOSS_ENGINE_PID_PATH`: engine pid file path (default `/tmp/boss-engine.pid`)
- `BOSS_ENGINE_FORCE_RESTART`: set `1` to force-restart the engine on app launch
- `BOSS_ENGINE_STOP_ON_EXIT`: set `1` to stop engine when app exits
- `BOSS_SHOW_SYSTEM_MESSAGES`: set `1` to include internal system status messages
- `BOSS_ENGINE_LOG_PATH`: log file path (default `/tmp/boss-engine.log`)
- `BOSS_ENGINE_AUDIT_PATH`: audit log file path (default
  `~/Library/Application Support/Boss/engine-audit.log`)
- `RUST_LOG`: tracing filter for engine logs (default `info,acp_stderr=debug`)

## Forensic / audit log

Every engine process appends one JSON line per lifecycle transition to
`~/Library/Application Support/Boss/engine-audit.log`:

- **`start`** — written before any work runs. Carries `pid`, `ppid`,
  `argv`, `parent_command` (best-effort `ps -o command=` of the
  parent), `engine_version`, `socket_paths` (the frontend and events
  sockets the engine *intends* to bind), `state_db_path`, and
  `prior_state_db_size`.
- **`socket_bound` / `socket_bind_failed`** — emitted at each
  `UnixListener::bind` site. `socket_kind` is `frontend` or `events`.
  Failures include the formatted error.
- **`shutdown`** — written when a graceful-shutdown signal fires
  (`reason="signal:SIGINT"` / `signal:SIGTERM`), when `app::run`
  returns an error (`reason="error:<first line>"`), or when the
  process panics (`reason="crash:<first line>"`). Carries
  `uptime_sec` derived from the start record. A `start` line with no
  matching `shutdown` is itself the signal that the prior instance
  died unrecoverably (e.g. `SIGKILL`).

The file is bounded at ~2 MiB; on overflow the engine drops the
oldest half on the next append. Inspect it with `tail` or
`jq -c . engine-audit.log | tail`. To override the path (tests,
out-of-tree installs) export `BOSS_ENGINE_AUDIT_PATH`.

## Agents mode (Phase 6a libghostty embedding)

The toolbar's **Agents** mode is a full-bleed 4 × 2 grid of `libghostty`
terminal panes (one per worker slot). Each pane runs `claude` directly
via libghostty `initial_input` — engine-driven spawn lands in Phase 6f.

The Boss pane is intentionally not here; per the V2 design it lives in
**Work** mode as a docked panel (Phase 7). Phase 6a ships only the
worker grid.

### Bootstrap

`GhosttyKit.xcframework` is **not** checked in. Build it locally first:

```bash
cd tools/boss/app-macos
./scripts/bootstrap-ghosttykit.sh
```

The script clones `ghostty-org/ghostty`, builds the macOS xcframework via
zig, and places it at `tools/boss/app-macos/ThirdParty/GhosttyKit.xcframework`.

Requirements: macOS 15+, Xcode Metal Toolchain (`xcodebuild
-downloadComponent MetalToolchain`), and `zig@0.15` (Homebrew preferred,
falls back to a cached download).

### Run

Agents-mode panes are currently SwiftPM-only:

```bash
cd tools/boss/app-macos
swift run Boss
```

The Bazel build does not include `Sources/Ghostty/*.swift`; under Bazel
the Agents tab shows a placeholder pointing at this section. The Work
tab is unaffected and continues to function in both build paths.

### Known limitation: claude folder-trust prompt

Each pane launches `claude` in `$HOME` with no leased workspace yet, so
on first run claude shows its interactive workspace-trust prompt
("Accept" / "Cancel"). Click Accept once per pane the first time; the
acceptance is persisted in `~/.claude.json` and won't repeat on
subsequent app launches. This goes away in Phase 6f, where each worker
runs in a leased cube workspace the user already trusts.
