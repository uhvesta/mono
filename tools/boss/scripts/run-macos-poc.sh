#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-macos-poc.sh

Starts the Boss macOS PoC app and auto-launches the engine.

Required environment variables:
  ANTHROPIC_API_KEY   API key for Claude (used for pane summaries).

Optional environment variables:
  BOSS_ENGINE_PID_PATH
  BOSS_ENGINE_LOG_PATH
  BOSS_SOCKET_PATH    Unix socket path (default /tmp/boss-engine.sock).
  BOSS_ENGINE_FORCE_RESTART   Set 1 to force-stop existing engine before launch.
  BOSS_ENGINE_AUTOSTART
  BOSS_ENGINE_STOP_ON_EXIT    Set 1 to stop engine when app exits.
  BOSS_SHOW_SYSTEM_MESSAGES   Set 1 to show internal system status lines in UI.
  BOSS_ENGINE_CMD
  BOSS_CUBE_CMD       How the engine invokes cube. Defaults to 'cube' (PATH-resolved).
                      On a dev machine without cube installed, set this to use the
                      bazel-built cube instead:
                        export BOSS_CUBE_CMD='bazel run //tools/cube:cube --'
  RUST_LOG
EOF
}

while (($# > 0)); do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ANTHROPIC_API_KEY is required." >&2
  echo "Example: export ANTHROPIC_API_KEY=... && $0" >&2
  exit 1
fi

for cmd in bazel; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing required command: $cmd" >&2
    exit 1
  fi
done

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../../.." && pwd)"

export BOSS_ENGINE_PID_PATH="${BOSS_ENGINE_PID_PATH:-/tmp/boss-engine.pid}"
export BOSS_ENGINE_LOG_PATH="${BOSS_ENGINE_LOG_PATH:-/tmp/boss-engine.log}"
export BOSS_ENGINE_FORCE_RESTART="${BOSS_ENGINE_FORCE_RESTART:-0}"
export BOSS_ENGINE_STOP_ON_EXIT="${BOSS_ENGINE_STOP_ON_EXIT:-0}"
export BOSS_SHOW_SYSTEM_MESSAGES="${BOSS_SHOW_SYSTEM_MESSAGES:-0}"
export RUST_LOG="${RUST_LOG:-info}"

echo "Launching Boss..."
echo "Repo: $repo_root"
echo "BOSS_ENGINE_PID_PATH: $BOSS_ENGINE_PID_PATH"
echo "BOSS_ENGINE_LOG_PATH: $BOSS_ENGINE_LOG_PATH"
echo "BOSS_ENGINE_FORCE_RESTART: $BOSS_ENGINE_FORCE_RESTART"
echo "BOSS_ENGINE_STOP_ON_EXIT: $BOSS_ENGINE_STOP_ON_EXIT"
echo "BOSS_SHOW_SYSTEM_MESSAGES: $BOSS_SHOW_SYSTEM_MESSAGES"
echo "RUST_LOG: $RUST_LOG"

cd "$repo_root"
exec bazel run //tools/boss/app-macos:Boss
