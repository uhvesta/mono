#!/usr/bin/env bash
# boss-release.sh — post-merge release step.
#
# Builds Boss.app with the three shake credentials embedded, zips it, and
# creates a GitHub Release on spinyfin/mono tagged boss-v1.0.N where N is one
# greater than the highest existing boss-v1.0.* release.
#
# Only triggered on the main branch (see pipeline.yml `if:` condition).
# Skips (exit 0) when the merge does not touch anything under tools/boss/.
# Retries the asset upload step on transient failures; the release record is created first.
#
# Secret sources (in priority order):
#   1. Env var already set (Pipeline Settings → Environment Variables).
#   2. Buildkite native secrets store via `buildkite-agent secret get`.
#
# See tools/boss/docs/buildkite-shake-secrets-setup.md for provisioning.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

log "[boss-release] releasing"
echo "[boss-release] agent: $(uname -a)"
echo "[boss-release] bazelisk: $(bazelisk version 2>&1 | head -1)"

# ── resolve last released tag (all trigger paths) ────────────────────────────
# Always resolve the last boss-v* tag and its commit SHA.  Both the
# idempotency guard (below) and the cron change-detection block need them,
# so we do the lookup once here unconditionally.

BUILDKITE_SOURCE="${BUILDKITE_SOURCE:-}"

log "[boss-release] resolving last boss-v* release tag"
LAST_TAG=$(gh release list --repo spinyfin/mono --limit 200 --json tagName \
  --jq '[.[] | select(.tagName | test("^boss-v1\\.0\\.[0-9]+$"))] | .[0].tagName' 2>/dev/null || true)

LAST_SHA=""
if [[ -n "${LAST_TAG}" ]]; then
  # BK checkouts are shallow (single-commit fetch, no --tags). Fetch the
  # specific release tag so git rev-list can resolve it locally.
  git fetch origin "refs/tags/${LAST_TAG}:refs/tags/${LAST_TAG}" 2>/dev/null || true

  LAST_SHA=$(git rev-list -n 1 "${LAST_TAG}" 2>/dev/null || true)

  if [[ -z "${LAST_SHA}" ]]; then
    # Local resolution still failed (annotated tag, fetch blocked, etc.).
    # Fall back to GitHub API — resolves both lightweight and annotated tags.
    echo "[boss-release] ${LAST_TAG} not in local refs; querying GitHub API"
    LAST_SHA=$(gh api "repos/spinyfin/mono/commits/${LAST_TAG}" \
      --jq '.sha' 2>/dev/null || true)
  fi
fi

# ── idempotency guard: never re-release an already-tagged commit (ALL paths) ─
# This check is SEPARATE from change-detection and applies to every trigger,
# including manual / API runs. Rationale: a manual re-trigger on an unchanged
# main branch would otherwise bump MAX_N and cut a new version on the same
# commit (observed as boss-v1.0.10 and boss-v1.0.11 both on commit 484ea18).
#
# If a deliberate re-cut is ever required, delete the tag first or use a
# dedicated --force flag; do NOT rely on manual triggering.
if [[ -n "${LAST_SHA}" ]]; then
  HEAD_SHA=$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")
  if [[ "${HEAD_SHA}" == "${LAST_SHA}" ]]; then
    echo "release step skipped: HEAD (${HEAD_SHA:0:12}) is already the commit for ${LAST_TAG} — re-releasing the same commit is a no-op"
    exit 0
  fi
fi

# ── guard: skip if no Boss-affecting changes (cron path only) ─────────────────
# For scheduled (cron) builds, only publish a release when there are
# Boss-affecting changes since the last boss-v* tag. A cron run with no Boss
# changes exits 0 silently.
#
# For manual triggers (BUILDKITE_SOURCE == "ui" or "api"), skip change
# detection entirely — the operator explicitly asked for a release.
#
# Paths that count as Boss-affecting:
#   - tools/boss/** — the binary's source code
#   - .buildkite/steps/boss-release.sh — the release script itself
#   - .buildkite/pipeline.yml — the release wiring

if [[ "${BUILDKITE_SOURCE}" == "ui" || "${BUILDKITE_SOURCE}" == "api" ]]; then
  echo "[boss-release] manual trigger via ${BUILDKITE_SOURCE}; skipping change-detection"
else
  log "[boss-release] checking for Boss-affecting changes since last tag"

  if [[ -z "${LAST_TAG}" ]]; then
    echo "[boss-release] no previous boss-v* tag found; proceeding with first release"
  elif [[ -z "${LAST_SHA}" ]]; then
    echo "[boss-release] WARNING: could not resolve tag ${LAST_TAG} by any means; proceeding"
  else
    # Unshallow if needed so git diff can reach LAST_SHA.
    if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
      echo "[boss-release] unshallowing repo for full diff"
      git fetch --unshallow origin 2>/dev/null || true
    fi

    TOUCHED=$(git diff --name-only "${LAST_SHA}..HEAD" 2>/dev/null || true)
    BOSS_TOUCHED=$(echo "${TOUCHED}" | grep -E "^(tools/boss/|\.buildkite/steps/boss-release\.sh|\.buildkite/pipeline\.yml)" || true)

    if [[ -z "${BOSS_TOUCHED}" ]]; then
      TOUCHED_SUMMARY=$(echo "${TOUCHED}" | tr '\n' ' ')
      echo "release step skipped: no Boss-affecting changes since ${LAST_TAG} (touched: ${TOUCHED_SUMMARY})"
      exit 0
    fi
    echo "[boss-release] Boss-affecting changes detected since ${LAST_TAG}; proceeding"
  fi
fi

# ── read secrets ──────────────────────────────────────────────────────────────

_read_secret() {
  local name="$1"
  # Honour a pre-set env var (Pipeline Settings or local override).
  if [[ -n "${!name:-}" ]]; then
    printf '%s' "${!name}"
    return 0
  fi
  # Buildkite native secrets store.
  if command -v buildkite-agent &>/dev/null; then
    buildkite-agent secret get "$name" 2>/dev/null || true
  fi
}

BOSS_SHAKE_APP_ID=$(_read_secret BOSS_SHAKE_APP_ID)
BOSS_SHAKE_INSTALLATION_ID=$(_read_secret BOSS_SHAKE_INSTALLATION_ID)
BOSS_SHAKE_PRIVATE_KEY_PEM=$(_read_secret BOSS_SHAKE_PRIVATE_KEY_PEM)
export BOSS_SHAKE_APP_ID BOSS_SHAKE_INSTALLATION_ID BOSS_SHAKE_PRIVATE_KEY_PEM

missing=()
[[ -z "${BOSS_SHAKE_APP_ID:-}" ]]           && missing+=("BOSS_SHAKE_APP_ID")
[[ -z "${BOSS_SHAKE_INSTALLATION_ID:-}" ]]  && missing+=("BOSS_SHAKE_INSTALLATION_ID")
[[ -z "${BOSS_SHAKE_PRIVATE_KEY_PEM:-}" ]]  && missing+=("BOSS_SHAKE_PRIVATE_KEY_PEM")

if (( ${#missing[@]} > 0 )); then
  die "Missing Buildkite secrets: ${missing[*]}
Set these in the Buildkite secrets store or in Pipeline Settings → Environment Variables.
See tools/boss/docs/buildkite-shake-secrets-setup.md for step-by-step instructions."
fi

echo "[boss-release] credentials loaded (APP_ID=[REDACTED])"

# ── fetch tags so workspace-status.sh gets a git-derived version ──────────────
# Buildkite clones are shallow by default and do not carry all remote tags.
# tools/boss/installer/workspace-status.sh relies on `git describe --tags
# --match "boss-v*"` to derive STABLE_BOSS_VERSION.  Without the tags that
# command returns empty and the version falls back to "0.0.0-dev-<sha>".
# Fetching all tags here guarantees the describe call works and means the
# binary embeds the real release version string (see version-tag section below).
log "[boss-release] fetching boss-v* tags for version stamping"
git fetch --tags origin 2>/dev/null || true

# ── compute next release version ─────────────────────────────────────────────
# Tags match boss-v1.0.N (monorepo-prefixed, mirrors checkleft-v* convention).
# If no matching release exists yet, start at boss-v1.0.0.
#
# IMPORTANT: this block is intentionally placed BEFORE the bazel build so that
# the next-version tag can be pushed to the remote before Bazel runs
# workspace-status.sh.  That lets `git describe --tags --match "boss-v*"
# --exact-match` hit the tag and stamp the binary with the exact release
# version (e.g. "1.0.5") rather than a dev suffix.

log "[boss-release] computing next version"
EXISTING_TAGS=$(gh release list --repo spinyfin/mono --limit 200 \
  --json tagName --jq '.[].tagName' 2>/dev/null || true)

MAX_N=-1
while IFS= read -r tag; do
  if [[ "${tag}" =~ ^boss-v1\.0\.([0-9]+)$ ]]; then
    n="${BASH_REMATCH[1]}"
    if (( n > MAX_N )); then MAX_N="${n}"; fi
  fi
done <<< "${EXISTING_TAGS}"

NEXT_N=$(( MAX_N + 1 ))
VERSION="boss-v1.0.${NEXT_N}"
ARTIFACT="Boss-1.0.${NEXT_N}.zip"
echo "[boss-release] version: ${VERSION}  artifact: ${ARTIFACT}"

# Push the release tag to the remote BEFORE building so that
# workspace-status.sh can resolve it via `git describe --exact-match` and
# stamp the binary with the clean "1.0.N" version string.
TAG_PUSHED=0
NOTES_FILE=""
WORK_DIR=""
RELEASE_CREATED=0

# Single EXIT trap — handles every failure path after the tag is pushed.
# Mirrors checkleft-release.sh's TAG_PUSHED-guarded cleanup pattern.
_cleanup() {
  [[ -n "${NOTES_FILE}" ]] && rm -f "${NOTES_FILE}"
  if [[ "${TAG_PUSHED}" == "1" && "${RELEASE_CREATED}" == "0" ]]; then
    echo "[boss-release] release not completed — deleting leaked remote tag ${VERSION}" >&2
    git push origin ":refs/tags/${VERSION}" 2>/dev/null || true
    git tag -d "${VERSION}" 2>/dev/null || true
  fi
  [[ -n "${WORK_DIR}" ]] && rm -rf "${WORK_DIR}"
}
trap '_cleanup' EXIT

log "[boss-release] creating and pushing release tag ${VERSION} (before build)"
git tag "${VERSION}" HEAD
git push origin "refs/tags/${VERSION}"
TAG_PUSHED=1

# ── GhosttyKit stub ───────────────────────────────────────────────────────────
# rules_swift_package_manager runs `swift package describe` during Bazel
# analysis; the stub lets SPM parse the Package.swift manifest without
# requiring a real GhosttyKit build.  Same setup as mac-app-build.sh.

XCFW="tools/boss/app-macos/ThirdParty/GhosttyKit.xcframework"
if [[ ! -f "${XCFW}/Info.plist" ]]; then
  log "[boss-release] creating GhosttyKit.xcframework stub for SPM describe"
  mkdir -p "${XCFW}/macos-arm64"
  cat > "${XCFW}/Info.plist" << 'PLIST_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AvailableLibraries</key>
    <array>
        <dict>
            <key>LibraryIdentifier</key>
            <string>macos-arm64</string>
            <key>LibraryPath</key>
            <string>GhosttyKit.a</string>
            <key>SupportedArchitectures</key>
            <array><string>arm64</string></array>
            <key>SupportedPlatform</key>
            <string>macos</string>
        </dict>
    </array>
    <key>CFBundlePackageType</key>
    <string>XFWK</string>
    <key>XCFrameworkFormatVersion</key>
    <string>1.0</string>
</dict>
</plist>
PLIST_EOF
  printf 'void GhosttyKit_stub(void) {}\n' | \
    xcrun clang -arch arm64 -x c - -c -o /tmp/ghosttykit_stub.o -mmacosx-version-min=15.0
  ar rcs "${XCFW}/macos-arm64/GhosttyKit.a" /tmp/ghosttykit_stub.o
fi

# ── build Boss.app (optimised, credentials embedded) ─────────────────────────
# Credentials are passed via --define so rules_rust includes them in the rustc
# compile action's cache key + env (option_env! reads them at compile time);
# --action_env alone does not affect the rustc action.
#
# CRITICAL: the build flags below (especially -c opt) change the output
# directory bazel-out is configured into. The path-discovery cquery MUST use
# the IDENTICAL flag set, otherwise it resolves a different configuration's
# output dir — specifically the credential-free `fastbuild` Boss.zip left
# behind by the mac-app-build step (`bazel build //tools/boss/app-macos/...`,
# no -c opt, no creds) — and the smoke test ends up verifying the wrong binary.
# That mismatch is exactly what made every prior fix attempt "pass locally" but
# fail in CI: the credentials were embedded correctly in the opt artifact, but
# the smoke test extracted the fastbuild one. Keep BUILD_FLAGS the single
# source of truth shared by both invocations.
BUILD_FLAGS=(
  -c opt
  --define=BOSS_SHAKE_APP_ID="$BOSS_SHAKE_APP_ID"
  --define=BOSS_SHAKE_INSTALLATION_ID="$BOSS_SHAKE_INSTALLATION_ID"
  --define=BOSS_SHAKE_PRIVATE_KEY_PEM="$BOSS_SHAKE_PRIVATE_KEY_PEM"
)

log "[boss-release] building //tools/boss/app-macos:Boss (opt)"
bazel build "${BUILD_FLAGS[@]}" //tools/boss/app-macos:Boss

# Discover the actual zip output path via cquery, using the SAME BUILD_FLAGS so
# the resolved path matches the configuration we just built (see note above).
log "[boss-release] discovering Boss.zip output path"
ZIP_PATH=$(bazel cquery "${BUILD_FLAGS[@]}" --output=files //tools/boss/app-macos:Boss 2>/dev/null | grep -E '\.zip$' | head -1)

if [[ -z "${ZIP_PATH}" ]]; then
  die "Unable to discover Boss.zip path via cquery. Contents of bazel-bin/tools/boss/app-macos/:
$(ls -la bazel-bin/tools/boss/app-macos/ 2>/dev/null || echo '(directory not found)')"
fi

[[ -f "${ZIP_PATH}" ]] || die "Boss.zip not found at discovered path: ${ZIP_PATH}"
echo "[boss-release] Boss.zip: ${ZIP_PATH}"

# ── prepare the pre-zipped artifact ────────────────────────────────────────────
# The macos_application rule pre-zips the bundle, so we just rename it to the
# release version and prepare it for publication.

log "[boss-release] preparing ${ARTIFACT}"
WORK_DIR=$(mktemp -d -t boss-release)

cp "${ZIP_PATH}" "${WORK_DIR}/${ARTIFACT}"
echo "[boss-release] artifact: $(du -sh "${WORK_DIR}/${ARTIFACT}" | cut -f1)"

# ── create GitHub Release ─────────────────────────────────────────────────────
# Split into three independent steps to isolate failure modes and enable
# selective retry on the (flaky) asset-upload step.

log "[boss-release] generating release notes for ${VERSION}"
NOTES_FILE="$(mktemp /tmp/boss-release-notes-XXXXXX.md)"
if [[ -n "${LAST_TAG}" ]]; then
  # Ensure full history for git log — a shallow clone silently truncates the
  # commit range returned by changelog, including on manual (ui/api) triggers
  # where the change-detection unshallow is skipped.
  if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
    echo "[boss-release] unshallowing repo for changelog"
    git fetch --unshallow origin 2>/dev/null || true
  fi
  bin/changelog \
    --project tools/boss/PROJECT.yaml \
    --from "${LAST_TAG}" \
    --to "${VERSION}" \
    --repo spinyfin/mono \
    --enrich \
    > "${NOTES_FILE}"
else
  printf 'Initial Boss release.\n' > "${NOTES_FILE}"
fi

log "[boss-release] creating GitHub Release ${VERSION}"
gh release create "${VERSION}" \
  --repo spinyfin/mono \
  --title "Boss ${VERSION#boss-v}" \
  --notes-file "${NOTES_FILE}"
RELEASE_CREATED=1

log "[boss-release] uploading asset with retry"
UPLOAD_OK=0
for attempt in 1 2 3; do
  if gh release upload "${VERSION}" "${WORK_DIR}/${ARTIFACT}" \
      --repo spinyfin/mono --clobber; then
    UPLOAD_OK=1
    break
  fi
  echo "[boss-release] upload attempt ${attempt} failed; sleeping $((attempt * 15))s before retry"
  sleep $((attempt * 15))
done

if (( UPLOAD_OK != 1 )); then
  die "release ${VERSION} created but asset upload failed after 3 attempts; manually upload via 'gh release upload ${VERSION} <path>' or delete the empty release with 'gh release delete ${VERSION}'"
fi

log "[boss-release] done — release ${VERSION} published"
