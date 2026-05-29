#!/usr/bin/env bash
# checks.sh — CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.).
# Always scoped to what changed via --base-ref=<base>. Never runs --all
# automatically. --all is manual-only, for catching/fixing pre-existing
# violations.
# Does not invoke jj; checkleft detects the git VCS automatically.
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

# Always scope to what changed. --all is manual-only.
if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    echo "[checks] PR build — scoping to changes against origin/${base_branch}"
    CHECKLEFT_ARGS=(run --base-ref="origin/${base_branch}")
else
    # Push-to-main or merge-queue build. Derive the merge-base against
    # origin/main so only this push's changes are checked.
    merge_base=$(git merge-base HEAD origin/main)
    echo "[checks] push/merge-queue build — scoping to changes since ${merge_base}"
    CHECKLEFT_ARGS=(run --base-ref="${merge_base}")
fi

bin/checkleft "${CHECKLEFT_ARGS[@]}"

echo "[checks] ok"
