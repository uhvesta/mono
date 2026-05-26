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

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

log "[boss-release] starting"
echo "[boss-release] agent: $(uname -a)"
echo "[boss-release] bazelisk: $(bazelisk version 2>&1 | head -1)"

# ── guard: skip if no Boss-affecting changes ──────────────────────────────────
# Only publish a release when the merge actually touched the Boss source tree or
# the release pipeline itself. A merge that only modifies checkleft, CI infra,
# docs, etc. should not produce a new Boss release.
#
# Paths that trigger a release:
#   - tools/boss/** — the binary's source code
#   - .buildkite/steps/boss-release.sh — the release script itself, so fixes can be validated
#   - .buildkite/pipeline.yml — the release wiring, so pipeline changes can be validated
#
# Note: this guard does NOT cover shared crates outside tools/boss/ — if such a
# dependency changes without a corresponding tools/boss/ change, the release is
# skipped and the next in-scope Boss merge will pick up the transitive change.

log "[boss-release] checking for Boss-affecting changes"
TOUCHED=$(git diff --name-only HEAD~1 HEAD 2>/dev/null || true)
BOSS_TOUCHED=$(echo "${TOUCHED}" | grep -E "^(tools/boss/|\.buildkite/steps/boss-release\.sh|\.buildkite/pipeline\.yml)" || true)

if [[ -z "${BOSS_TOUCHED}" ]]; then
  TOUCHED_SUMMARY=$(echo "${TOUCHED}" | tr '\n' ' ')
  echo "release step skipped: no Boss-affecting changes in this merge (touched: ${TOUCHED_SUMMARY})"
  exit 0
fi
echo "[boss-release] Boss-affecting changes detected, proceeding"

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
# .bazelrc already declares --action_env for all three BOSS_SHAKE_* vars so
# Bazel uses a different cache key from the credential-free mac-app-build step.

log "[boss-release] building //tools/boss/app-macos:Boss (opt)"
bazel build -c opt //tools/boss/app-macos:Boss

# Discover the actual zip output path via cquery (defensive against rule changes).
log "[boss-release] discovering Boss.zip output path"
ZIP_PATH=$(bazel cquery --output=files //tools/boss/app-macos:Boss 2>/dev/null | grep -E '\.zip$' | head -1)

if [[ -z "${ZIP_PATH}" ]]; then
  die "Unable to discover Boss.zip path via cquery. Contents of bazel-bin/tools/boss/app-macos/:
$(ls -la bazel-bin/tools/boss/app-macos/ 2>/dev/null || echo '(directory not found)')"
fi

[[ -f "${ZIP_PATH}" ]] || die "Boss.zip not found at discovered path: ${ZIP_PATH}"
echo "[boss-release] Boss.zip: ${ZIP_PATH}"

# ── compute next release version ─────────────────────────────────────────────
# Tags match boss-v1.0.N (monorepo-prefixed, mirrors checkleft-v* convention).
# If no matching release exists yet, start at boss-v1.0.0.

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
BOSS_BIN=$(bazel cquery --output=files //tools/boss/cli:boss -c opt 2>/dev/null | head -1)
[[ -x "${BOSS_BIN}" ]] || die "boss binary not found for smoke test (cquery returned: '${BOSS_BIN}')"

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
This means the just-built binary does not have shake credentials embedded.
Check that the three BOSS_SHAKE_* secrets are set in the BK pipeline and that
.bazelrc propagates them via --action_env."
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
