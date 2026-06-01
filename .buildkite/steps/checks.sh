#!/usr/bin/env bash
# checks.sh — CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.).
# Scoped to what changed — checkleft classifies the environment automatically.
# --all is manual-only, for catching/fixing pre-existing violations.
#
# checkleft is invoked via repobin (bin/checkleft) rather than `bazel run` so
# that the binary runs with the repository root as its working directory.
# `bazel run` sets the process cwd to the Bazel runfiles tree, which causes
# checkleft to miss CHECKS.* config files; repobin builds the target and then
# execs the binary directly, preserving the caller's cwd.
set -euo pipefail

echo "--- [checks] starting"

echo "--- [checks] installing repobin tools into bin/"
bazel build --config=ci-linux-disk-cache //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

bin/checkleft run

echo "[checks] ok"
