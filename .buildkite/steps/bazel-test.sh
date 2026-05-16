#!/usr/bin/env bash
# bazel-test.sh — bazel test //... (canonical rust + integration test step).
# With P1 landed, covers engine lib tests via rust_test(crate=":engine_lib").
# macOS-only targets (//tools/boss/app-macos/... and //tools/boss/installer/...)
# are excluded here; no mac test targets exist yet, but excluding keeps the
# pattern consistent with bazel-build.sh so Linux agents never attempt Swift
# toolchain resolution.
set -euo pipefail

echo "--- [bazel-test] starting"
echo "[bazel-test] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel test --test_output=errors --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...

echo "[bazel-test] ok"
