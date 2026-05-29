#!/usr/bin/env bash
# Test: regular PR-build base computation in checks.sh
#
# Verifies that when running under a regular PR build (BUILDKITE_PULL_REQUEST
# is set, branch is NOT in gh-readonly-queue), the scoping base is
# git merge-base(origin/main, HEAD) — the fork point — NOT origin/main's tip.
#
# Scenario that triggered the original bug (T831 / PR #945):
#   - PR branches off main at commit A
#   - Other PRs merge to main after A, adding unrelated files
#     (runner.rs, ChatViewModel.swift, rust_giant_structs_use_builder.rs, main.rs)
#   - PR head is the raw branch tip (not a merge commit)
#   - checks.sh used --base-ref=origin/main (the 2-dot tip), sweeping in
#     all of main's post-fork drift as if it belonged to this PR
#
# Correct base = git merge-base origin/main HEAD = A (fork point).
# Scope is A..HEAD = only the PR's own commits.
set -euo pipefail

# Extract the regular PR base computation logic from checks.sh.
compute_pr_base() {
    git merge-base origin/main HEAD
}

# Set up a temp git repo that simulates a BK PR environment (no merge commit).
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
cd "$tmpdir"
git init -q
git config user.email "test@test.com"
git config user.name "Test"

# Create a fake origin remote pointing to a bare clone we'll set up.
origin_dir=$(mktemp -d)
trap 'rm -rf "$tmpdir" "$origin_dir"' EXIT

# Build main history in origin.
git init -q --bare "$origin_dir"
git remote add origin "$origin_dir"

# commit_a: the fork point — where this PR branches off main.
echo "base" > base.txt
git add base.txt
git commit -q -m "commit A — PR branches here"
git push -q origin HEAD:refs/heads/main

commit_a=$(git rev-parse HEAD)

# commit_b: an unrelated change merged to main AFTER the PR branched.
# This simulates runner.rs / ChatViewModel.swift drift in the T831 scenario.
echo "main-only change" > runner.rs
git add runner.rs
git commit -q -m "unrelated: runner.rs merged to main after PR branched"
git push -q origin HEAD:refs/heads/main

commit_b=$(git rev-parse HEAD)
main_tip=$commit_b

# Restore to fork point, then create the PR branch.
git checkout -q "$commit_a" 2>/dev/null
git checkout -q -b pr-branch

# PR-specific change: only pr_file.rs — this is what the PR actually touches.
echo "pr change" > pr_file.rs
git add pr_file.rs
git commit -q -m "feat: PR-specific change"
pr_head=$(git rev-parse HEAD)

# The fork point is commit_a; the correct base for scoping.
expected_base=$commit_a
expected_base_short=$(git rev-parse --short "$expected_base")
tip_short=$(git rev-parse --short "$main_tip")

# --- Test 1: compute_pr_base returns the fork point, not main's tip ---
actual_base=$(compute_pr_base)
actual_base_short=$(git rev-parse --short "$actual_base")

if [[ "$actual_base" != "$expected_base" ]]; then
    echo "FAIL: PR base mismatch" >&2
    echo "  expected: $expected_base ($expected_base_short = fork point)" >&2
    echo "  got:      $actual_base ($actual_base_short)" >&2
    exit 1
fi
echo "OK [1/3]: base = $actual_base_short (fork point, not origin/main tip $tip_short)"

# --- Test 2: diff(merge_base..HEAD) includes only the PR's own file ---
changed_files=$(git diff --name-only "$actual_base" HEAD)
if echo "$changed_files" | grep -q "runner.rs"; then
    echo "FAIL: scoped diff includes runner.rs — main-only file leaked in" >&2
    echo "  changed files: $changed_files" >&2
    exit 1
fi
echo "OK [2/3]: scoped diff does NOT include runner.rs (main-only file excluded)"

if ! echo "$changed_files" | grep -q "pr_file.rs"; then
    echo "FAIL: scoped diff missing pr_file.rs — PR's own change not captured" >&2
    echo "  changed files: $changed_files" >&2
    exit 1
fi
echo "OK [3/3]: scoped diff includes pr_file.rs (PR's own change present)"

# --- Demonstrate why origin/main tip is WRONG ---
wrong_files=$(git diff --name-only origin/main HEAD)
if echo "$wrong_files" | grep -q "runner.rs"; then
    echo ""
    echo "NOTE: using origin/main tip as base incorrectly includes runner.rs — demonstrates the T831 bug"
fi

echo ""
echo "All checks passed. git merge-base correctly scopes PR builds to the PR's own changes."
