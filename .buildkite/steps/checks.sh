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

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "+++ installing repobin tools into bin/"
bazel build //tools/repobin:repobin

./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

echo "--- [checks] running checks"
bin/checkleft run
