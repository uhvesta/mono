#!/usr/bin/env bash
# integrity-bazel.sh — full-repo bazel build + test on macOS.
#
# Runs bazel build //... followed by bazel test //... on a macos-arm64 agent.
# Unlike the PR pipeline's bazel-build/bazel-test steps (Linux, Swift excluded),
# this step runs on macOS and covers the full //... target set — including
# //tools/boss/app-macos/... and //tools/boss/installer/... that require the
# Swift/macOS toolchain.
#
# GhosttyKit stub: rules_swift_package_manager runs `swift package describe`
# during Bazel analysis.  A stub xcframework satisfies the SPM manifest parse
# without requiring a real GhosttyKit build.  Same setup as mac-app-build.sh
# and boss-release.sh.
set -euo pipefail

echo "--- [integrity-bazel] starting"
echo "[integrity-bazel] agent: $(uname -a)"
echo "[integrity-bazel] bazelisk: $(bazelisk version 2>&1 | head -1)"

XCFW="tools/boss/app-macos/ThirdParty/GhosttyKit.xcframework"
if [[ ! -f "${XCFW}/Info.plist" ]]; then
  echo "[integrity-bazel] creating GhosttyKit.xcframework stub for SPM describe"
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

echo "--- [integrity-bazel] bazel build //..."
bazel build --verbose_failures --keep_going //...

echo "--- [integrity-bazel] bazel test //..."
bazel test --test_output=errors --keep_going //...

echo "[integrity-bazel] ok"
