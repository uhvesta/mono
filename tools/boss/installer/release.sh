#!/usr/bin/env bash
# release.sh — signs, notarizes, and staples the Boss installer .pkg.
#
# Usage (via bazel run — do not invoke directly):
#
#   Full release (signs + notarizes):
#     bazel run //tools/boss/installer:release --config=release
#
#   Skip notarization — CI validation path, produces a codesigned .pkg:
#     bazel run //tools/boss/installer:release --config=release -- --skip-notary
#
#   Dry-run — print all commands without executing anything beyond setup:
#     bazel run //tools/boss/installer:release --config=release -- --dry-run
#
# Signing identity env vars (required unless identities can be auto-detected):
#   BOSS_DEVELOPER_ID_APPLICATION  e.g. "Developer ID Application: Jane Doe (TEAMID)"
#   BOSS_DEVELOPER_ID_INSTALLER    e.g. "Developer ID Installer: Jane Doe (TEAMID)"
#
# Notarization env vars (required unless --skip-notary):
#   AC_KEYCHAIN_PROFILE            keychain profile name stored via:
#                                  xcrun notarytool store-credentials <profile>
#   OR all three of:
#   AC_USERNAME                    Apple ID email
#   AC_PASSWORD                    app-specific password
#   AC_TEAM_ID                     10-character team ID
#
# TODO(@brian,2026-09-01): (task_18af2b6ce51452e0_1c) Replace env-var credential
# reads with Boss-managed Keychain storage once the notary-credentials chore lands.
# That chore will add a `boss release-credentials store` command that calls
# `xcrun notarytool store-credentials` and records the profile name in
# ~/Library/Application Support/Boss/release-config.toml; this script will
# then read the profile name from there instead of from AC_KEYCHAIN_PROFILE.
#
# Implementation follows design doc Q2 "Order matters":
#   1. codesign every Mach-O with Developer ID Application + hardened runtime
#   2. codesign Boss.app bundle
#   3. codesign --verify --deep --strict + spctl --assess (fail-fast)
#   4. pkgbuild → productbuild → productsign with Developer ID Installer
#   5. xcrun notarytool submit --wait (unless --skip-notary)
#   6. xcrun stapler staple (unless --skip-notary)

set -euo pipefail

# ── argument parsing ──────────────────────────────────────────────────────────

SKIP_NOTARY=false
DRY_RUN=false

for arg in "$@"; do
  case "$arg" in
    --skip-notary) SKIP_NOTARY=true ;;
    --dry-run)     DRY_RUN=true; SKIP_NOTARY=true ;;
    *) echo "ERROR: Unknown argument: $arg" >&2; exit 1 ;;
  esac
done

# ── helpers ───────────────────────────────────────────────────────────────────

die()  { echo "ERROR: $*" >&2; exit 1; }
log()  { echo "==> $*"; }
warn() { echo "WARN: $*" >&2; }

# Execute a command, or print it in dry-run mode.
run() {
  if $DRY_RUN; then
    printf '[dry-run]'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

# ── resolve runfiles ──────────────────────────────────────────────────────────
# Bazel sets RUNFILES_DIR when invoking a sh_binary via `bazel run`.
# With Bzlmod the canonical repository name for the main workspace is "_main"
# (not the module name from MODULE.bazel).

if [[ -n "${RUNFILES_DIR:-}" ]]; then
  WS="${RUNFILES_DIR}/_main"
elif [[ -f "${RUNFILES_MANIFEST_FILE:-/dev/null}" ]]; then
  die "Manifest-based runfiles are not supported; run via 'bazel run'."
else
  # Fallback: runfiles tree adjacent to the script itself.
  WS="${BASH_SOURCE[0]}.runfiles/_main"
fi

PKG_INSTALLER="tools/boss/installer"

PAYLOAD_DIR="${WS}/${PKG_INSTALLER}/boss_pkg_payload"
PKG_UNSIGNED_DIR="${WS}/${PKG_INSTALLER}/boss_pkg_unsigned"
SCRIPTS_DIR="${WS}/${PKG_INSTALLER}/scripts"
APP_ENTITLEMENTS="${WS}/${PKG_INSTALLER}/entitlements/app.entitlements"
ENGINE_ENTITLEMENTS="${WS}/${PKG_INSTALLER}/entitlements/engine.entitlements"
DISTRIBUTION_XML="${WS}/${PKG_INSTALLER}/distribution.xml"

for path in "$PAYLOAD_DIR" "$PKG_UNSIGNED_DIR" "$SCRIPTS_DIR" \
            "$APP_ENTITLEMENTS" "$ENGINE_ENTITLEMENTS" "$DISTRIBUTION_XML"; do
  [[ -e "$path" ]] || die "Missing runfile: $path"
done

# ── resolve the git SHA from the unsigned .pkg filename ───────────────────────

UNSIGNED_PKG=$(find "$PKG_UNSIGNED_DIR" -maxdepth 1 -name "Boss-*.pkg" | head -1)
[[ -n "$UNSIGNED_PKG" ]] || die "No Boss-*.pkg found in $PKG_UNSIGNED_DIR"
PKG_BASENAME=$(basename "$UNSIGNED_PKG")
SHA="${PKG_BASENAME#Boss-}"
SHA="${SHA%.pkg}"
log "Git SHA: $SHA"

if [[ "$SHA" == *"-dirty" ]]; then
  warn "Building from a dirty working copy; notarization will be skipped."
  SKIP_NOTARY=true
fi

# ── resolve signing identities ────────────────────────────────────────────────

APP_IDENTITY="${BOSS_DEVELOPER_ID_APPLICATION:-}"
INSTALLER_IDENTITY="${BOSS_DEVELOPER_ID_INSTALLER:-}"

if [[ -z "$APP_IDENTITY" ]]; then
  APP_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
    | grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"' || true)
  [[ -n "$APP_IDENTITY" ]] || die \
    "Set BOSS_DEVELOPER_ID_APPLICATION or add a 'Developer ID Application' cert to your keychain."
  warn "Auto-detected application identity: $APP_IDENTITY"
fi

if [[ -z "$INSTALLER_IDENTITY" ]]; then
  INSTALLER_IDENTITY=$(security find-identity -v 2>/dev/null \
    | grep -o '"Developer ID Installer: [^"]*"' | head -1 | tr -d '"' || true)
  [[ -n "$INSTALLER_IDENTITY" ]] || die \
    "Set BOSS_DEVELOPER_ID_INSTALLER or add a 'Developer ID Installer' cert to your keychain."
  warn "Auto-detected installer identity: $INSTALLER_IDENTITY"
fi

# ── determine output path ─────────────────────────────────────────────────────

OUTPUT_DIR="${BUILD_WORKSPACE_DIRECTORY:-.}"
OUTPUT_PKG="${OUTPUT_DIR}/Boss-${SHA}.pkg"

log "Application identity : $APP_IDENTITY"
log "Installer identity   : $INSTALLER_IDENTITY"
log "Output               : $OUTPUT_PKG"
$SKIP_NOTARY && log "Notarization         : SKIPPED (--skip-notary)"

# ── stage Boss.app into a writable scratch tree ───────────────────────────────

SCRATCH=$(mktemp -d -t boss-release)
trap 'rm -rf "$SCRATCH"' EXIT

PAYLOAD_STAGING="${SCRATCH}/payload"
mkdir -p "$PAYLOAD_STAGING"

log "Staging Boss.app from payload..."
cp -R "${PAYLOAD_DIR}/Boss.app" "$PAYLOAD_STAGING/"
chmod -R u+w "$PAYLOAD_STAGING/Boss.app"

APP="${PAYLOAD_STAGING}/Boss.app"

# ── step 1: codesign every Mach-O (inside-out) ───────────────────────────────
# Sign leaf binaries first (dylibs, helpers), then the bundle envelope last.
# See design doc §Q2 "Order matters".

log "Signing GhosttyKit framework binaries..."
FRAMEWORKS_DIR="${APP}/Contents/Frameworks"
if [[ -d "$FRAMEWORKS_DIR" ]]; then
  # Sign each Mach-O inside the Frameworks directory (dylibs, framework binaries).
  while IFS= read -r -d '' macho; do
    run codesign --force --options runtime \
      -s "$APP_IDENTITY" "$macho"
  done < <(find "$FRAMEWORKS_DIR" -type f -print0 | while IFS= read -r -d '' f; do
    file "$f" | grep -q "Mach-O" && printf '%s\0' "$f"
  done)
fi

log "Signing bundled CLI binaries..."
BIN_DIR="${APP}/Contents/Resources/bin"
if [[ -d "$BIN_DIR" ]]; then
  # engine needs the engine entitlements (empty, but explicit for forward-compat).
  [[ -f "${BIN_DIR}/engine" ]] && \
    run codesign --force --options runtime \
      --entitlements "$ENGINE_ENTITLEMENTS" \
      -s "$APP_IDENTITY" "${BIN_DIR}/engine"

  # boss, bossctl, boss-event are plain Rust CLIs with no special entitlements.
  for bin in boss bossctl boss-event; do
    [[ -f "${BIN_DIR}/${bin}" ]] && \
      run codesign --force --options runtime \
        -s "$APP_IDENTITY" "${BIN_DIR}/${bin}"
  done
fi

# ── step 2: codesign Boss.app bundle ─────────────────────────────────────────
# Signing the bundle re-seals the code-signature directory and covers the main
# executable (Contents/MacOS/Boss) and all previously signed sub-components.

log "Signing Boss.app bundle..."
run codesign --force --options runtime \
  --entitlements "$APP_ENTITLEMENTS" \
  -s "$APP_IDENTITY" "$APP"

# ── step 3: verify signatures (fail-fast) ────────────────────────────────────

log "Verifying signatures..."
run codesign --verify --deep --strict --verbose=2 "$APP"

if ! $DRY_RUN; then
  # spctl may warn about missing notarization ticket — that is expected here.
  # We treat non-zero as a warning rather than a hard failure so that --skip-notary
  # builds are still usable even though they haven't been notarized yet.
  spctl --assess --verbose "$APP" 2>&1 || \
    warn "spctl --assess returned non-zero — normal before notarization."
fi

# ── step 4a: pkgbuild — component package ────────────────────────────────────

COMPONENT_PKG="${SCRATCH}/BossComponent.pkg"
log "Running pkgbuild..."
run pkgbuild \
  --root "$PAYLOAD_STAGING" \
  --identifier dev.spinyfin.boss.installer \
  --install-location ~/Applications \
  --scripts "$SCRIPTS_DIR" \
  --version "0+${SHA}" \
  "$COMPONENT_PKG"

# ── step 4b: productbuild — distribution package ─────────────────────────────
# Wraps the component .pkg with a distribution XML that enables the
# currentUserHomeDirectory domain, ensuring Installer.app offers "Install for
# me only" by default and never requests an admin password.

DISTRIBUTION_PKG="${SCRATCH}/BossDistribution.pkg"
log "Running productbuild..."
run productbuild \
  --distribution "$DISTRIBUTION_XML" \
  --package-path "$SCRATCH" \
  "$DISTRIBUTION_PKG"

# ── step 4c: productsign — sign distribution .pkg ────────────────────────────

SIGNED_PKG="${SCRATCH}/BossSigned.pkg"
log "Running productsign..."
run productsign \
  --sign "$INSTALLER_IDENTITY" \
  "$DISTRIBUTION_PKG" \
  "$SIGNED_PKG"

# ── steps 5–6: notarize + staple (skipped when --skip-notary) ────────────────

if $SKIP_NOTARY; then
  log "Skipping notarization (--skip-notary)."
  if ! $DRY_RUN; then
    cp "$SIGNED_PKG" "$OUTPUT_PKG"
  fi
else
  # TODO(@brian,2026-09-01): (task_18af2b6ce51452e0_1c) Replace env-var credential
  # reads with a Keychain profile managed by `boss release-credentials store`.  Once that
  # chore lands, prefer reading the profile name from
  # ~/Library/Application Support/Boss/release-config.toml and fall back to
  # AC_KEYCHAIN_PROFILE for CI / script overrides.
  NOTARY_ARGS=()
  if [[ -n "${AC_KEYCHAIN_PROFILE:-}" ]]; then
    NOTARY_ARGS+=(--keychain-profile "$AC_KEYCHAIN_PROFILE")
  elif [[ -n "${AC_USERNAME:-}" && -n "${AC_PASSWORD:-}" && -n "${AC_TEAM_ID:-}" ]]; then
    NOTARY_ARGS+=(--apple-id "$AC_USERNAME" --password "$AC_PASSWORD" --team-id "$AC_TEAM_ID")
  else
    die "Notarization requires AC_KEYCHAIN_PROFILE, or all of AC_USERNAME + AC_PASSWORD + AC_TEAM_ID."
  fi

  log "Submitting to Apple notary service (this may take a few minutes)..."
  run xcrun notarytool submit --wait "${NOTARY_ARGS[@]}" "$SIGNED_PKG"

  log "Stapling notarization ticket..."
  run xcrun stapler staple "$SIGNED_PKG"

  if ! $DRY_RUN; then
    cp "$SIGNED_PKG" "$OUTPUT_PKG"
  fi
fi

# ── report ────────────────────────────────────────────────────────────────────

if $DRY_RUN; then
  log "Dry-run complete — no files written."
else
  log "Final artifact: $OUTPUT_PKG"
  shasum -a 256 "$OUTPUT_PKG"
fi
