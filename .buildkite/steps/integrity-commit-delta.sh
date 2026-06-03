#!/usr/bin/env bash
# integrity-commit-delta.sh — setup step for mono-integrity pipeline.
#
# Dynamically uploads the parallel integrity-bazel and integrity-checkleft
# steps via buildkite-agent pipeline upload. The integrity check always runs
# unconditionally; bazel caches make re-running cheap.
set -euo pipefail

echo "--- [commit-delta] uploading"

HEAD_SHA=$(git rev-parse HEAD)
echo "[commit-delta] HEAD: ${HEAD_SHA}"

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
PIPELINE

echo "[commit-delta] steps uploaded"
