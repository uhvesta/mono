#!/usr/bin/env bash
# Test: merge-queue base computation in checks.sh
#
# Verifies that when running under a gh-readonly-queue branch, the scoping
# base is HEAD^1 (the main tip the PR is merged onto), NOT the fork point
# returned by `git merge-base HEAD^1 HEAD^2`.
#
# Scenario that triggered the original bug (T774 / PR #910):
#   - PR was branched from main at commit A (35 commits behind main's tip B)
#   - Other PRs merged to main between A and B, adding github_oauth.rs
#   - GitHub merge queue creates: HEAD = merge(B, pr_head)
#   - git merge-base HEAD^1 HEAD^2 = A (fork point, 35 commits behind B)
#   - Result: checkleft scoped A..HEAD, sweeping in github_oauth.rs from B
#
# Correct base = HEAD^1 = B (main tip). Scope is HEAD^1..HEAD = only PR's changes.
set -euo pipefail

# Extract the merge-queue base computation logic from checks.sh.
compute_merge_queue_base() {
    local parent_count
    parent_count=$(git log -1 --format="%P" HEAD | wc -w | tr -d ' ')
    if [[ "$parent_count" -ge 2 ]]; then
        git rev-parse HEAD^1
    else
        git merge-base HEAD origin/main
    fi
}

# Set up a temp git repo.
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
cd "$tmpdir"
git init -q
git config user.email "test@test.com"
git config user.name "Test"

# commit_a: the commit from which PR #910 branched (an older main commit).
echo "a" > a.txt
git add a.txt
git commit -q -m "commit A — PR #910 branches here"
commit_a=$(git rev-parse HEAD)

# commit_b: an unrelated PR merged to main AFTER PR #910 branched.
# This simulates the github_oauth.rs change in the T774 scenario.
echo "unrelated" > github_oauth.rs
git add github_oauth.rs
git commit -q -m "unrelated merged PR (e.g. github_oauth.rs changes)"
commit_b=$(git rev-parse HEAD)
main_tip=$commit_b

# PR branch: branched from commit_a, adds only checks.sh.
git checkout -q -b pr-branch "$commit_a"
echo "checks change" > checks.sh
git add checks.sh
git commit -q -m "fix(checks): PR #910 change"
pr_head=$(git rev-parse HEAD)

# Merge commit: simulate what GitHub creates in the merge queue.
#   HEAD^1 = main_tip (commit_b) — the main tip this PR is merged onto
#   HEAD^2 = pr_head  — this PR's tip
git checkout -q -b "gh-readonly-queue/main/pr-910-abc123" "$main_tip"
git merge -q --no-ff "$pr_head" -m "Merge pr-910 into queue"

# The correct base: HEAD^1 = main tip (commit_b).
expected_base=$commit_b
expected_base_short=$(git rev-parse --short "$expected_base")

# --- Test 1: base == HEAD^1 (main tip) ---
actual_base=$(compute_merge_queue_base)
actual_base_short=$(git rev-parse --short "$actual_base")

if [[ "$actual_base" != "$expected_base" ]]; then
    echo "FAIL: merge-queue base mismatch" >&2
    echo "  expected: $expected_base ($expected_base_short = HEAD^1, main tip)" >&2
    echo "  got:      $actual_base ($actual_base_short)" >&2
    exit 1
fi
echo "OK [1/3]: base = $actual_base_short (HEAD^1, correct main tip)"

# --- Test 2: diff(HEAD^1..HEAD) includes only the PR's own file (checks.sh) ---
changed_files=$(git diff --name-only HEAD^1 HEAD)
if echo "$changed_files" | grep -q "github_oauth.rs"; then
    echo "FAIL: scoped diff HEAD^1..HEAD includes github_oauth.rs — unrelated merged PR leaked in" >&2
    echo "  changed files: $changed_files" >&2
    exit 1
fi
echo "OK [2/3]: scoped diff HEAD^1..HEAD does NOT include github_oauth.rs"

if ! echo "$changed_files" | grep -q "checks.sh"; then
    echo "FAIL: scoped diff HEAD^1..HEAD is missing checks.sh — PR's own change not captured" >&2
    echo "  changed files: $changed_files" >&2
    exit 1
fi
echo "OK [3/3]: scoped diff HEAD^1..HEAD includes checks.sh (PR's own change)"

# --- Demonstrate why merge-base is WRONG ---
fork_point=$(git merge-base HEAD^1 HEAD^2)
fork_point_short=$(git rev-parse --short "$fork_point")
if [[ "$fork_point" == "$expected_base" ]]; then
    echo "NOTE: in this test, fork point == main tip (both equal $fork_point_short)." >&2
    echo "      The bug only manifests when the PR is branched behind main's tip." >&2
else
    wrong_files=$(git diff --name-only "$fork_point" HEAD)
    echo "NOTE: git merge-base HEAD^1 HEAD^2 = $fork_point_short (fork point, != main tip $expected_base_short)"
    if echo "$wrong_files" | grep -q "github_oauth.rs"; then
        echo "NOTE: using fork point as base incorrectly includes github_oauth.rs — demonstrates the T774 bug"
    fi
fi

echo ""
echo "All checks passed. HEAD^1 correctly scopes merge-queue builds to the PR's own changes."
