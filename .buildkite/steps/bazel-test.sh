#!/usr/bin/env bash
# bazel-test.sh — bazel test //... (canonical rust + integration test step).
# With P1 landed, covers engine lib tests via rust_test(crate=":engine_lib").
# macOS-only targets (//tools/boss/app-macos/..., //tools/boss/installer/...)
# are excluded here; they run on the mac-app-build step on a macos-arm64 agent.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [bazel-test] testing"
echo "[bazel-test] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel test --test_output=errors --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...

echo "[bazel-test] ok"
