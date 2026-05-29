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
elif [[ "${BUILDKITE_BRANCH:-}" == gh-readonly-queue/* ]]; then
    # GitHub merge-queue build. HEAD is a merge commit created by GitHub:
    #   HEAD^1 = accumulated queue state before this PR
    #   HEAD^2 = this PR's original head
    #
    # Using git merge-base HEAD origin/main is wrong here: when HEAD^2 is a
    # descendant of HEAD^1 (the PR was based on a more recent main than what
    # the queue was seeded from), the common ancestor predates the PR's own
    # base, causing checkleft to scope over intermediate commits from other
    # PRs (including their Rust files) and flag phantom violations.
    #
    # Fix: scope to git merge-base HEAD^2 origin/main, which is the commit
    # where this PR's branch diverged from main — ignoring unrelated commits
    # that were ahead of the queue seed point.
    parent_count=$(git log -1 --format="%P" HEAD | wc -w | tr -d ' ')
    if [[ "$parent_count" -ge 2 ]]; then
        pr_head=$(git rev-parse HEAD^2)
        merge_base=$(git merge-base "$pr_head" origin/main)
        echo "[checks] merge-queue build — scoping to PR changes since ${merge_base}"
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
