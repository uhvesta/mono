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
elif [[ "${BUILDKITE_BRANCH:-}" == gh-readonly-queue/* ]]; then
    # GitHub merge-queue build. HEAD is a merge commit created by GitHub:
    #   HEAD^1 = the main tip this PR is being merged onto
    #   HEAD^2 = this PR's original head
    #
    # The correct base is HEAD^1 — the main tip the PR is merged onto.
    # Scoping HEAD^1..HEAD captures exactly what this PR contributes and
    # nothing else.
    #
    # Using git merge-base HEAD^1 HEAD^2 is WRONG: it returns the fork point
    # where the PR branched off main, which is potentially many commits behind
    # HEAD^1. That sweeps in every unrelated change other PRs merged to main
    # since this PR branched, inflating the diff with files this PR never
    # touched (e.g. github_oauth.rs in T774/PR#910).
    parent_count=$(git log -1 --format="%P" HEAD | wc -w | tr -d ' ')
    if [[ "$parent_count" -ge 2 ]]; then
        merge_base=$(git rev-parse HEAD^1)
        echo "[checks] merge-queue build — scoping to PR changes against HEAD^1 (${merge_base})"
    else
        # Unexpected: queue HEAD is not a merge commit. Fall back to the
        # naive merge-base so we still produce a useful scope.
        merge_base=$(git merge-base HEAD origin/main)
        echo "[checks] merge-queue build (non-merge HEAD) — scoping to changes since ${merge_base}"
    fi
    CHECKLEFT_ARGS=(run --base-ref="${merge_base}")
else
    # Push-to-main build. Derive the merge-base against origin/main so only
    # this push's changes are checked.
    merge_base=$(git merge-base HEAD origin/main)
    echo "[checks] push/main build — scoping to changes since ${merge_base}"
    CHECKLEFT_ARGS=(run --base-ref="${merge_base}")
fi

bin/checkleft "${CHECKLEFT_ARGS[@]}"

echo "[checks] ok"
