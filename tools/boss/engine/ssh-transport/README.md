# boss-ssh-transport

`boss-ssh-transport` owns the SSH plumbing that lets the Boss engine
dispatch and observe workers on remote hosts. It manages the lifecycle of
a persistent OpenSSH `ControlMaster` connection per remote host and
exposes the small set of operations the engine's `SshHostAdapter` needs:
running commands, pushing files, and wiring up the reverse socket that
carries worker hook events back to the engine. It is a leaf crate with no
internal Boss dependencies; only `boss-engine` depends on it.

## Architecture

The central type is `SshTransport`, which represents one multiplexed SSH
connection to a single host. Rather than re-authenticating on every call,
it opens a backgrounded `ControlMaster` (`ssh -M -N -f`) bound to an
engine-owned control socket, then funnels all subsequent traffic —
`ssh` command runs, `scp` pushes, and `-O forward`/`-O cancel`
control-channel requests — through that one socket. The invariant is
"one multiplex per host": even reverse forwards ride the existing master
instead of opening a second session. Opening is idempotent, probing an
existing socket with `ssh -O check` before deciding whether to reuse or
re-bind it.

A key collaboration is the reverse Unix-socket forward. The transport asks
the master to forward a remote socket path back to the engine's local
events socket, so a worker's `boss-event` shim can write to what looks
like a local socket on the remote host and have those bytes surface on the
engine side. This is the conceptual link to `boss-event` and the engine's
event-handling path.

Socket ownership is deliberate: control sockets live under an
engine-owned directory (`$BOSS_RUNTIME_DIR/ssh`, falling back to
`$HOME/.boss-remote-control`) rather than inside the user's `~/.ssh`, so
the engine can scrub its own SSH state — for example when a host is
removed — without disturbing the user's. Because a crashed engine can
leave dangling sockets that block a fresh master from binding, the crate
provides a startup sweep that unlinks stale `cm-*.sock` files.

Failures are classified rather than swallowed: command and `scp` output is
captured as an `SshOutput`, and stderr is mapped into coarse failure kinds
(disk full, permission denied, connection lost, or unclassified) so the
engine can attribute a remote failure to the right reason without parsing
raw text itself. Operations are time-budgeted with per-operation timeouts
so an unreachable host fails fast instead of hanging a dispatch.

## Scope

This crate intentionally does not implement reconnect-on-drop, retry
policy, or probe/interrupt/stop handling — those concerns live in the
engine. It is a thin, well-bounded transport layer: it carries bytes and
classifies outcomes, and leaves higher-level recovery and lifecycle
decisions to its caller.
