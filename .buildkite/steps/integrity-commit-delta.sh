#!/usr/bin/env bash
# integrity-commit-delta.sh — short-circuit for mono-integrity scheduled runs.
#
# For scheduled builds: fetch the mono-integrity-last-run tag and compare its
# SHA to HEAD.  If unchanged, exit 0 immediately — no new commits means no
# work to do.  For manual (UI/API) triggers, skip change detection and always
# proceed so operators can force a full check on demand.
#
# When checks are needed, dynamically uploads the parallel integrity-bazel and
# integrity-checkleft steps plus the tag-update step via buildkite-agent
# pipeline upload.
set -euo pipefail

echo "--- [commit-delta] starting"

BUILDKITE_SOURCE="${BUILDKITE_SOURCE:-}"
LAST_RUN_TAG="mono-integrity-last-run"

HEAD_SHA=$(git rev-parse HEAD)
echo "[commit-delta] HEAD: ${HEAD_SHA}"

if [[ "${BUILDKITE_SOURCE}" == "schedule" ]]; then
  echo "[commit-delta] scheduled build — checking for new commits since last run"

  # Fetch the last-run tag; fails silently when the tag does not yet exist.
  git fetch origin "refs/tags/${LAST_RUN_TAG}:refs/tags/${LAST_RUN_TAG}" 2>/dev/null || true

  LAST_SHA=$(git rev-list -n 1 "${LAST_RUN_TAG}" 2>/dev/null || true)

  if [[ -z "${LAST_SHA}" ]]; then
    echo "[commit-delta] no ${LAST_RUN_TAG} tag found; treating as first run"
  elif [[ "${HEAD_SHA}" == "${LAST_SHA}" ]]; then
    echo "[commit-delta] no new commits since last run (${LAST_SHA:0:12}); skipping"
    exit 0
  else
    echo "[commit-delta] new commits detected (last: ${LAST_SHA:0:12}); proceeding"
  fi
else
  echo "[commit-delta] manual trigger (source=${BUILDKITE_SOURCE:-unknown}); skipping change detection"
fi

echo "[commit-delta] uploading integrity check steps"

buildkite-agent pipeline upload << 'PIPELINE'
steps:
  - label: ":bazel: integrity-bazel"
    key: "integrity-bazel"
    command: ".buildkite/steps/integrity-bazel.sh"
    agents:
      queue: "macos-arm64"
    artifact_paths:
      - "bazel-out/**/extra_action_outputs/**/*"
      - "bazel-testlogs/**/test.log"
      - "bazel-testlogs/**/test.xml"

  - label: ":white_check_mark: integrity-checkleft"
    key: "integrity-checkleft"
    command: ".buildkite/steps/integrity-checkleft.sh"
    agents:
      queue: "bazel-any"

  - wait: ~
    continue_on_failure: false

  - label: ":bookmark: update-last-run"
    key: "integrity-tag-update"
    command: ".buildkite/steps/integrity-tag-update.sh"
    agents:
      queue: "bazel-any"
PIPELINE

echo "[commit-delta] steps uploaded"
