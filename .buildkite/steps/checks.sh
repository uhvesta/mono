#!/usr/bin/env bash
# checks.sh — CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.).
# On PR builds: scoped to changed files via --base_ref=origin/<base-branch>.
# On push-to-main builds: runs --all.
# Does not invoke jj; checkleft detects the git VCS automatically.
set -euo pipefail

echo "--- [checks] starting"

# Determine run scope: PR builds scope to changed files; main builds run all.
if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    echo "[checks] PR build — scoping to changes against origin/${base_branch}"
    CHECKLEFT_ARGS=(run --base_ref="origin/${base_branch}")
else
    echo "[checks] push build — running all checks"
    CHECKLEFT_ARGS=(run --all)
fi

bazel run //tools/checkleft -- "${CHECKLEFT_ARGS[@]}"

echo "[checks] ok"
