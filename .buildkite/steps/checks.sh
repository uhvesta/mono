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
    # BK checkouts are shallow by default. Unshallow so that git merge-base can
    # reach the fork point between the PR branch and origin/<base_branch>.
    # Without full history the merge-base computation fails and checkleft would
    # either error or fall back to diffing against the tip, mis-attributing
    # origin/<base_branch> drift to this PR.
    if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
        echo "[checks] shallow repo detected; unshallowing for merge-base computation"
        git fetch --unshallow origin 2>/dev/null || true
    fi
    CHECKLEFT_ARGS=(run --base-ref="origin/${base_branch}")
else
    echo "[checks] push build — running all checks"
    CHECKLEFT_ARGS=(run --all)
fi

bazel run --config=ci-linux-disk-cache //tools/checkleft -- "${CHECKLEFT_ARGS[@]}"

echo "[checks] ok"
