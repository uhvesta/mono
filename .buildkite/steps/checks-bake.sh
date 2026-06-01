#!/usr/bin/env bash
# TEMPORARY: checks-bake.sh — bake-period parity check (P844 migration step 2).
#
# Runs change-detection BOTH ways during the bake window and asserts that the
# resolved base SHA (and changed-file count) match:
#
#   • Legacy path:  base SHA derived in this script (same logic as checks.sh)
#                   and passed to checkleft via --base-ref.
#   • Auto path:    checkleft classifies the environment itself with no args.
#
# If the two paths disagree the step fails so we catch divergence before the
# shell scoping is removed.  This file is intentionally throwaway — remove it
# (and the pipeline step that calls it) once checks.sh scoping is retired.
#
# See: P844 / checkleft self-sufficient change detection project.

set -euo pipefail

echo "--- [checks-bake] starting (TEMPORARY bake-period parity check)"

echo "--- [checks-bake] installing repobin tools into bin/"
bazel build --config=ci-linux-disk-cache //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

# ── 1. Derive the legacy base SHA (same logic as checks.sh) ──────────────────

if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    echo "[checks-bake] PR build — fetching origin/${base_branch} for merge-base"
    git fetch origin "${base_branch}"
    if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
        echo "[checks-bake] shallow repo detected; unshallowing"
        git fetch --unshallow origin 2>/dev/null || true
    fi
    legacy_base=$(git merge-base "origin/${base_branch}" HEAD)
    legacy_scenario="pull-request(${base_branch})"

elif [[ "${BUILDKITE_BRANCH:-}" == gh-readonly-queue/* ]]; then
    parent_count=$(git log -1 --format="%P" HEAD | wc -w | tr -d ' ')
    if [[ "$parent_count" -ge 2 ]]; then
        legacy_base=$(git rev-parse HEAD^1)
        legacy_scenario="merge-queue"
    else
        git fetch origin main
        legacy_base=$(git merge-base HEAD origin/main)
        legacy_scenario="merge-queue(non-merge-fallback)"
    fi
    echo "[checks-bake] merge-queue build — legacy base: ${legacy_base}"

else
    git fetch origin main
    legacy_base=$(git merge-base HEAD origin/main)
    legacy_scenario="push-to-default"
    echo "[checks-bake] push/main build — legacy base: ${legacy_base}"
fi

echo "[checks-bake] legacy scenario: ${legacy_scenario}"
echo "[checks-bake] legacy base sha: ${legacy_base}"

# ── 2. Resolve both plans via checkleft show-plan ────────────────────────────

echo "--- [checks-bake] running legacy path (--base-ref=${legacy_base})"
legacy_output=$(bin/checkleft show-plan --base-ref="${legacy_base}" 2>/dev/null)
echo "[checks-bake] legacy checkleft output:"
echo "$legacy_output" | sed 's/^/  /'

echo "--- [checks-bake] running auto-classification path (no scoping args)"
auto_output=$(bin/checkleft show-plan 2>/dev/null)
echo "[checks-bake] auto checkleft output:"
echo "$auto_output" | sed 's/^/  /'

# ── 3. Parse and compare ─────────────────────────────────────────────────────

parse_field() {
    local key="$1"
    local output="$2"
    echo "$output" | grep "^${key}=" | cut -d= -f2-
}

legacy_sha=$(parse_field base_sha "$legacy_output")
auto_sha=$(parse_field base_sha "$auto_output")
legacy_files=$(parse_field changed_files "$legacy_output")
auto_files=$(parse_field changed_files "$auto_output")
auto_scenario=$(parse_field scenario "$auto_output")

echo "--- [checks-bake] parity comparison"
echo "[checks-bake] legacy base_sha:    ${legacy_sha:-<none>}"
echo "[checks-bake] auto   base_sha:    ${auto_sha:-<none>}"
echo "[checks-bake] legacy changed_files: ${legacy_files:-<none>}"
echo "[checks-bake] auto   changed_files: ${auto_files:-<none>}"
echo "[checks-bake] auto   scenario:    ${auto_scenario:-<none>}"

diverged=0

if [[ -z "${auto_sha}" ]]; then
    echo "[checks-bake] ERROR: auto path did not produce a base_sha (got: $(echo "$auto_output" | head -1))"
    diverged=1
elif [[ -z "${legacy_sha}" ]]; then
    echo "[checks-bake] ERROR: legacy path did not produce a base_sha (got: $(echo "$legacy_output" | head -1))"
    diverged=1
elif [[ "${auto_sha}" != "${legacy_sha}" ]]; then
    echo "[checks-bake] ERROR: base_sha DIVERGENCE"
    echo "[checks-bake]   legacy: ${legacy_sha}"
    echo "[checks-bake]   auto:   ${auto_sha}"
    diverged=1
fi

if [[ "${auto_files}" != "${legacy_files}" ]]; then
    echo "[checks-bake] WARNING: changed_files count differs (legacy=${legacy_files:-?} auto=${auto_files:-?})"
    # Not a hard failure — changed_files can differ if the auto path
    # includes untracked files; base_sha equality is the definitive check.
fi

if [[ "$diverged" -eq 0 ]]; then
    echo "[checks-bake] ok — auto path matches legacy (base_sha=${auto_sha} files=${auto_files:-?})"
else
    echo "[checks-bake] FAILED — auto and legacy paths disagree; see above"
    exit 1
fi
