#!/usr/bin/env bash
# integrity-tag-update.sh — advance mono-integrity-last-run to HEAD.
#
# Called after both integrity-bazel and integrity-checkleft pass (via the
# wait step in the dynamically-uploaded pipeline).  The tag is read by
# integrity-commit-delta.sh at the start of each scheduled run to detect
# whether new commits exist and skip the expensive checks when nothing changed.
set -euo pipefail

echo "--- [integrity-tag-update] starting"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "[integrity-tag-update] ERROR: GITHUB_TOKEN is unset; cannot push tag" >&2
  exit 1
fi

LAST_RUN_TAG="mono-integrity-last-run"
HEAD_SHA=$(git rev-parse HEAD)
echo "[integrity-tag-update] advancing ${LAST_RUN_TAG} → ${HEAD_SHA:0:12}"

git tag -f "${LAST_RUN_TAG}" HEAD
git push "https://x-access-token:${GITHUB_TOKEN}@github.com/spinyfin/mono" -f "refs/tags/${LAST_RUN_TAG}"

echo "[integrity-tag-update] done"
