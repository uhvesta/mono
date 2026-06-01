# boss-client

`boss-client` is the typed RPC client that Boss command-line tools use to
talk to the engine over its frontend Unix-domain socket. It owns the
connection mechanics — engine discovery, optional autostart, connecting,
and correlating requests with responses — so each CLI can issue
`FrontendRequest`s and get back `FrontendEvent`s without re-implementing
the wire protocol. It exists to keep that one set of rules in a single
place shared by the `boss` CLI, `bossctl`, and the engine itself.

## How it fits

The crate sits between the protocol definitions in `boss-protocol` and the
front-end binaries that drive the engine. `BossClient` is a
single-connection client: it sends a framed-JSON request envelope tagged
with a generated `request_id`, then reads engine events until it sees the
one carrying the matching id. It depends only on `boss-protocol` for the
request/event types, deliberately avoiding any dependency on `boss-engine`
itself — small on-disk shapes the engine also defines (such as the
control-token file) are duplicated here rather than imported, so a CLI
never has to pull in the whole engine crate.

`Discovery` captures everything needed to find and, if necessary, launch
the engine: the socket path, the PID-file path, whether autostart is
allowed, the resolved engine command, and timeouts. The engine-command
resolver is the most involved piece — it walks an ordered chain of sources
(explicit `BOSS_ENGINE_CMD`/`BOSS_ENGINE_BIN` overrides, a workspace
`bazel-bin` build, a sibling binary next to the running executable, and
finally a bare `boss-engine` on `PATH`), recording every step it tried so a
failed autostart can explain exactly how it got there. The resolver is
written as a pure function over explicit inputs so tests can exercise it
deterministically without touching process environment.

Stopping the engine prefers a token-authenticated `Shutdown` RPC — the same
authority the macOS app uses — and falls back to `SIGTERM` only when the RPC
path is unavailable, giving a developer a recoverable kill switch for a
wedged engine on a non-standard layout.

## Consumers

`boss-engine`, `bossctl`, and the `boss` CLI all depend on this crate to
reach a running engine; it depends on `boss-protocol` for the shared types.
