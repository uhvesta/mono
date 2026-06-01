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
#   BOSS_INITIAL_INPUT_FILE — path to a file holding the initial prompt.
#                            Preferred over BOSS_INITIAL_INPUT so a
#                            multi-KB prompt never has to survive ssh
#                            argv re-quoting. Read as claude's first
#                            positional arg (its initial user message).
#   BOSS_INITIAL_INPUT     — inline fallback for the initial prompt.
#
# Contract (output): the worker is launched DETACHED (`nohup` +
# background) so it survives the engine restarting and the launching
# SSH session closing. Its stdout/stderr are teed to
# `<workspace>/.boss/worker.log` so the engine can read recent output
# on demand over the multiplex. The wrapper's own exit status reports
# only *launch* success (0) or a sentinel config/toolchain failure
# (78-81) — the worker's real lifecycle is driven by its hook events
# over the forwarded BOSS_EVENTS_SOCKET, not by this wrapper blocking.
# The wrapper prints `boss-remote-run: starting … pid=<n>` to stderr so
# the engine can record `work_runs.remote_pid`.
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

# Per-run scratch + log dir under the cube workspace. The engine pulls
# tails of worker.log over the SSH multiplex on demand (a later phase)
# so remote runs get the same recent-output surface as local panes —
# without a second reverse channel.
boss_run_dir="$BOSS_WORKSPACE/.boss"
mkdir -p "$boss_run_dir" 2>/dev/null || true
worker_log="$boss_run_dir/worker.log"

# Resolve the initial prompt. A file (BOSS_INITIAL_INPUT_FILE) is the
# engine's preferred channel; an inline value is the fallback. Claude
# Code treats its first positional arg as the initial user message —
# mirroring the local pane, which launches with `claude "$(cat …)"`.
initial_input=""
if [ -n "${BOSS_INITIAL_INPUT_FILE:-}" ] && [ -f "$BOSS_INITIAL_INPUT_FILE" ]; then
    initial_input="$(cat "$BOSS_INITIAL_INPUT_FILE")"
elif [ -n "${BOSS_INITIAL_INPUT:-}" ]; then
    initial_input="$BOSS_INITIAL_INPUT"
fi

# Launch DETACHED so the worker survives the engine restarting and the
# launching SSH session closing: `nohup` makes it ignore the SIGHUP the
# remote sshd sends on session teardown, and backgrounding reparents it
# off this wrapper. stdin is taken from /dev/null (the prompt rides the
# positional arg) and stdout+stderr are teed to the per-run log. The
# wrapper returns immediately; the worker keeps running.
if [ -n "$initial_input" ]; then
    nohup claude --dangerously-skip-permissions "$initial_input" \
        >"$worker_log" 2>&1 </dev/null &
else
    nohup claude --dangerously-skip-permissions \
        >"$worker_log" 2>&1 </dev/null &
fi
worker_pid=$!

# Echo the embedded version + worker pid so the engine sees the wrapper
# that actually ran (separate from --version, a probe-only path) and can
# record `work_runs.remote_pid`. Prefixed `boss-remote-run:` so the
# engine can recognize it amongst stderr noise without a structured
# handshake.
printf 'boss-remote-run: starting run_id=%s version=%s pid=%s\n' \
    "$BOSS_RUN_ID" "$BOSS_REMOTE_RUN_VERSION" "$worker_pid" 1>&2

exit 0
