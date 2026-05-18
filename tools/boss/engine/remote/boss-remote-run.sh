#!/bin/sh
# boss-remote-run — engine-owned wrapper invoked on remote hosts by the
# Boss SSH adapter to launch a worker. Owned by the engine; deployed
# via SSH/scp from `bossctl hosts add` (eager) and on dispatch when the
# embedded version drifts from what the engine expects (lazy).
#
# Contract (input via env vars):
#   BOSS_RUN_ID            — engine-assigned run id; spliced into hook events.
#   BOSS_EVENTS_SOCKET     — path to the forwarded engine events socket
#                            (typically a SSH-remote-forwarded Unix socket
#                            living under /tmp/ on this host).
#   BOSS_LEASE_ID          — cube lease id (already leased by the engine
#                            prior to invoking this wrapper; passed
#                            through to the worker process env so the
#                            shim can stamp it on every event).
#   BOSS_WORKSPACE         — absolute workspace path on this host.
#   BOSS_REPO_REMOTE_URL   — repo origin URL (used by the worker for
#                            informational logging only; cube already
#                            cloned the repo before lease was issued).
#   BOSS_INITIAL_INPUT     — initial prompt to feed to claude on stdin.
#
# Contract (output): claude's stdout/stderr is streamed back over the
# SSH channel. The worker process group's exit status determines
# success/failure on the engine side.
#
# --version: print the embedded BOSS_REMOTE_RUN_VERSION and exit 0.
# Used by the engine for the lazy version-handshake at dispatch time.

set -u

# Engine writes the canonical version string here at build time; if
# this file ships unstamped the literal sentinel below makes the drift
# obvious to a reader. Engine pushes a fresh copy on every mismatch.
BOSS_REMOTE_RUN_VERSION="__BOSS_REMOTE_RUN_VERSION__"

if [ "${1:-}" = "--version" ]; then
    printf '%s\n' "$BOSS_REMOTE_RUN_VERSION"
    exit 0
fi

# Validate the contract before doing anything destructive. Missing
# variables are an engine bug, not a user-visible failure mode, so we
# print a short diagnostic that the SSH transport will surface back to
# the engine as the wrapper exit-status reason.
required_vars="BOSS_RUN_ID BOSS_EVENTS_SOCKET BOSS_LEASE_ID BOSS_WORKSPACE"
for var in $required_vars; do
    eval "val=\${$var:-}"
    if [ -z "$val" ]; then
        printf 'boss-remote-run: required env var %s is unset\n' "$var" 1>&2
        exit 78  # EX_CONFIG: incorrect configuration
    fi
done

if [ ! -d "$BOSS_WORKSPACE" ]; then
    printf 'boss-remote-run: workspace path does not exist: %s\n' "$BOSS_WORKSPACE" 1>&2
    exit 78
fi

# Health check: claude must be reachable. The engine sets a documented
# sentinel exit code so the failure surface in `last_error_text` is
# clean (`host_missing_claude` per the Q6 design table).
if ! command -v claude >/dev/null 2>&1; then
    printf 'boss-remote-run: `claude` not found on PATH; install or set up the worker toolchain\n' 1>&2
    exit 79  # documented sentinel: claude missing
fi

# cube must be reachable for the same reason. The engine leases the
# workspace before invoking the wrapper, but the worker may still
# invoke `cube` for status/heartbeat, so we fail-fast on missing tool.
if ! command -v cube >/dev/null 2>&1; then
    printf 'boss-remote-run: `cube` not found on PATH; install cube on this host\n' 1>&2
    exit 80  # documented sentinel: cube missing
fi

# gh must be reachable for PR creation. The engine catches expired
# tokens at heartbeat time (Phase 5) but we still need the binary present.
if ! command -v gh >/dev/null 2>&1; then
    printf 'boss-remote-run: `gh` not found on PATH; install gh on this host\n' 1>&2
    exit 81  # documented sentinel: gh missing
fi

cd "$BOSS_WORKSPACE" || {
    printf 'boss-remote-run: cd into %s failed\n' "$BOSS_WORKSPACE" 1>&2
    exit 78
}

# The shim binary on this host ships under cube's umbrella. The
# engine relies on the local cube install having put `boss-event` on
# the worker's PATH via cube's standard install. We export the env
# vars `boss-event` reads so each hook fires with the engine's
# correlation token and lease id stamped on it.
export BOSS_RUN_ID
export BOSS_EVENTS_SOCKET
export BOSS_LEASE_ID
export BOSS_WORKSPACE
export BOSS_REPO_REMOTE_URL="${BOSS_REPO_REMOTE_URL:-}"

# Echo the embedded version so the engine sees the wrapper that
# actually ran (separate from --version, which is a probe-only path).
# Prefixed `boss-remote-run:` so the engine can recognize it amongst
# stderr noise without a structured handshake.
printf 'boss-remote-run: starting run_id=%s version=%s\n' \
    "$BOSS_RUN_ID" "$BOSS_REMOTE_RUN_VERSION" 1>&2

# Hand the worker process the prompt via stdin if one was provided.
# Otherwise exec claude with no piped input; the engine's local
# behavior is to bring up an interactive claude pane, but on the
# remote we run headless and stream stdout/stderr back.
if [ -n "${BOSS_INITIAL_INPUT:-}" ]; then
    printf '%s' "$BOSS_INITIAL_INPUT" | exec claude --dangerously-skip-permissions
else
    exec claude --dangerously-skip-permissions
fi
