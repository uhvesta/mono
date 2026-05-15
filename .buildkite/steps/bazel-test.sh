#!/usr/bin/env bash
# bazel-test.sh — bazel test //... (canonical rust + integration test step).
# With P1 landed, covers engine lib tests via rust_test(crate=":engine_lib").
set -euo pipefail

echo "--- [bazel-test] starting"
echo "[bazel-test] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel test //... --config=ci

echo "[bazel-test] ok"
