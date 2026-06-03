#!/usr/bin/env bash
# mac-app-build.sh — build and test macOS Swift targets on a macos-arm64 agent.
# Linux agents have no Swift toolchain; this step runs on Zakalwe-1 instead.
# Also builds the installer/pkg targets whose boss_pkg_payload rule transitively
# depends on //tools/boss/app-macos:Boss and therefore requires macOS.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [mac-app-build] starting"
echo "[mac-app-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

# rules_swift_package_manager's swift_deps module extension runs
# `swift package describe` during every Bazel analysis.  The Package.swift
# declares a .binaryTarget(path: "ThirdParty/GhosttyKit.xcframework") for
# SPM-based dev builds; that path is gitignored and built by
# scripts/bootstrap-ghosttykit.sh.  On CI we only need a stub so that SPM
# can parse the manifest — the actual Bazel build uses @ghostty_kit from the
# http_archive defined in MODULE.bazel, not this path.
XCFW="tools/boss/app-macos/ThirdParty/GhosttyKit.xcframework"
if [[ ! -f "${XCFW}/Info.plist" ]]; then
  echo "[mac-app-build] creating GhosttyKit.xcframework stub for SPM describe"
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

bazel build //tools/boss/app-macos/... //tools/boss/installer/...
# Run every macOS Swift test target, not just BossTests, so the UpdateCore
# module's tests (UpdateChecker / UpdateDownloader — the self-update download,
# verification, quarantine-strip, and staging logic) gate merges too. The `...`
# wildcard picks up both //tools/boss/app-macos:BossTests and
# //tools/boss/app-macos/Tests/UpdateCore:UpdateTests.
bazel test --test_output=errors //tools/boss/app-macos/...

echo "[mac-app-build] ok"
