#!/usr/bin/env bash
# integrity-checkleft.sh — run checkleft over the entire repo (--all).
#
# Unlike checks.sh in the PR pipeline (which scopes to changed paths), this
# step checks the full repo to surface pre-existing violations that per-PR
# diff-scoped runs would miss.
#
# checkleft is invoked via repobin so that the binary runs with the repository
# root as its working directory — same rationale as checks.sh.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [integrity-checkleft] checking"

echo "--- [integrity-checkleft] installing repobin tools into bin/"
bazel build //tools/repobin:repobin

export REPOBIN_BAZEL_FLAGS="--config=ci"
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

echo "--- [integrity-checkleft] running checkleft --all"
bin/checkleft run --all

