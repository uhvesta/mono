#!/usr/bin/env bash
# bazel-build.sh — bazel build //... (dependency-graph compile guard).
# Catches visibility violations, missing deps, and broken generated files.
set -euo pipefail

echo "--- [bazel-build] starting"
echo "[bazel-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel build //... --config=ci

echo "[bazel-build] ok"
