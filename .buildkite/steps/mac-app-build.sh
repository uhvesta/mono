#!/usr/bin/env bash
# mac-app-build.sh — build macOS Swift targets on a macos-arm64 agent.
# Linux agents have no Swift toolchain; this step runs on Zakalwe-1 instead.
# Also builds the installer/pkg targets whose boss_pkg_payload rule transitively
# depends on //tools/boss/app-macos:Boss and therefore requires macOS.
set -euo pipefail

echo "--- [mac-app-build] starting"
echo "[mac-app-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel build //tools/boss/app-macos/... //tools/boss/installer/...

echo "[mac-app-build] ok"
