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

log "[boss-release] starting"
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
# Register a trap so a failed build cleans up the leaked tag.
TAG_PUSHED=0
_cleanup_tag() {
  if (( TAG_PUSHED == 1 )); then
    echo "[boss-release] build failed after tagging — deleting remote tag ${VERSION}"
    git push origin ":refs/tags/${VERSION}" 2>/dev/null || true
    git tag -d "${VERSION}" 2>/dev/null || true
  fi
}
trap '_cleanup_tag' ERR

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

# The build succeeded; cancel the tag-cleanup trap.
trap - ERR

# ── prepare the pre-zipped artifact ────────────────────────────────────────────
# The macos_application rule pre-zips the bundle, so we just rename it to the
# release version and prepare it for publication.

log "[boss-release] preparing ${ARTIFACT}"
WORK_DIR=$(mktemp -d -t boss-release)
trap 'rm -rf "${WORK_DIR}"' EXIT

cp "${ZIP_PATH}" "${WORK_DIR}/${ARTIFACT}"
echo "[boss-release] artifact: $(du -sh "${WORK_DIR}/${ARTIFACT}" | cut -f1)"

# ── smoke test: verify shake credentials are embedded in the binary ───────────
# File a live test issue against spinyfin/mono using the just-built boss binary,
# confirm it succeeds (proves credentials are embedded), then delete it.
# Runs before the GitHub Release is created so a credential failure aborts the
# release rather than producing a published-but-broken artifact.

log "[boss-release] smoke test: verifying embedded shake credentials"
SMOKE_DIR=$(mktemp -d -t boss-smoke)
trap 'rm -rf "${SMOKE_DIR}"' RETURN
ditto -x -k "${WORK_DIR}/${ARTIFACT}" "${SMOKE_DIR}/extracted"
BOSS_BIN="${SMOKE_DIR}/extracted/Boss.app/Contents/Resources/bin/boss"
[[ -x "${BOSS_BIN}" ]] || die "boss binary not found in shipped artifact at expected path: ${BOSS_BIN}
Contents of extracted Boss.app/Contents/Resources/bin/:
$(ls -la "${SMOKE_DIR}/extracted/Boss.app/Contents/Resources/bin/" 2>/dev/null || echo '(directory not found)')"

SMOKE_MD="${WORK_DIR}/smoke.md"
cat > "${SMOKE_MD}" << 'SMOKE_EOF'
[boss-shake smoke test] release pipeline credential verification

Automatically filed by the boss-release BK step to verify that embedded GitHub
App credentials work in the just-built binary. Deleted immediately after creation.
SMOKE_EOF

SHAKE_OUT=$("${BOSS_BIN}" shake --json "${SMOKE_MD}" 2>&1) || true
ISSUE_NUM=$(printf '%s' "${SHAKE_OUT}" | jq -r '.number // empty' 2>/dev/null || true)
if [[ -z "${ISSUE_NUM}" ]]; then
  die "smoke test FAILED — boss shake did not return an issue number.
Output: ${SHAKE_OUT}
The extracted binary (from ${ZIP_PATH}) does not have shake credentials embedded.
Check, in order: (1) the three BOSS_SHAKE_* secrets are set/non-empty in the BK
pipeline; (2) ZIP_PATH above resolves to a '-opt-' output dir, NOT '-fastbuild-'
— if it shows fastbuild, the discovery cquery and the build are using different
flags and the smoke test is verifying the credential-free mac-app-build artifact;
(3) the rust_binary embeds the values via rustc_env/option_env! (tools/boss/cli)."
fi
echo "[boss-release] smoke test passed: filed test issue #${ISSUE_NUM}"
gh issue delete "${ISSUE_NUM}" --repo spinyfin/mono --yes
echo "[boss-release] smoke test issue #${ISSUE_NUM} deleted"

# ── create GitHub Release ─────────────────────────────────────────────────────
# Split into three independent steps to isolate failure modes and enable
# selective retry on the (flaky) asset-upload step.

log "[boss-release] creating GitHub Release ${VERSION}"
gh release create "${VERSION}" \
  --repo spinyfin/mono \
  --title "Boss ${VERSION#boss-v}" \
  --generate-notes

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
