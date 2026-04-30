#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE_DIR="$ROOT_DIR/.build-cache"
UPSTREAM_DIR="$CACHE_DIR/ghostty-upstream"
OUTPUT_DIR="$ROOT_DIR/ThirdParty"
FRAMEWORK_DIR="$OUTPUT_DIR/GhosttyKit.xcframework"
TOOLCHAIN_DIR="$CACHE_DIR/toolchains"
ZIG_VERSION="0.15.2"

case "$(uname -m)" in
  arm64) TARGET_TRIPLE="aarch64-macos.15.0" ;;
  x86_64) TARGET_TRIPLE="x86_64-macos.15.0" ;;
  *)
    echo "unsupported macOS architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

mkdir -p "$CACHE_DIR" "$OUTPUT_DIR" "$TOOLCHAIN_DIR"

ensure_metal_toolchain() {
  if xcrun metal -v >/dev/null 2>&1; then
    return 0
  fi

  cat >&2 <<'EOF'
missing Xcode Metal Toolchain component

Install it with:
  xcodebuild -downloadComponent MetalToolchain
EOF
  exit 1
}

ensure_zig() {
  if command -v brew >/dev/null 2>&1; then
    local brew_prefix
    brew_prefix="$(brew --prefix zig@0.15 2>/dev/null || true)"
    if [[ -n "$brew_prefix" && -x "$brew_prefix/bin/zig" ]]; then
      echo "$brew_prefix/bin/zig"
      return 0
    fi
  fi

  if command -v zig >/dev/null 2>&1; then
    command -v zig
    return 0
  fi

  local arch
  case "$(uname -m)" in
    arm64) arch="aarch64" ;;
    x86_64) arch="x86_64" ;;
  esac

  local archive="zig-${arch}-macos-${ZIG_VERSION}.tar.xz"
  local url="https://ziglang.org/download/${ZIG_VERSION}/${archive}"
  local archive_path="$TOOLCHAIN_DIR/$archive"
  local extract_dir="$TOOLCHAIN_DIR/zig-${arch}-macos-${ZIG_VERSION}"
  local zig_bin="$extract_dir/zig"

  if [[ ! -x "$zig_bin" ]]; then
    rm -f "$archive_path"
    curl -L "$url" -o "$archive_path"
    rm -rf "$extract_dir"
    tar -xf "$archive_path" -C "$TOOLCHAIN_DIR"
  fi

  echo "$zig_bin"
}

ensure_metal_toolchain
ZIG_BIN="$(ensure_zig)"
SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"

if [[ ! -d "$UPSTREAM_DIR/.git" ]]; then
  git clone --depth 1 https://github.com/ghostty-org/ghostty "$UPSTREAM_DIR"
else
  git -C "$UPSTREAM_DIR" fetch --depth 1 origin main
  git -C "$UPSTREAM_DIR" reset --hard origin/main
fi

(
  cd "$UPSTREAM_DIR"
  SDKROOT="$SDKROOT" \
  MACOSX_DEPLOYMENT_TARGET=15.0 \
  "$ZIG_BIN" build \
    -Dtarget="$TARGET_TRIPLE" \
    -Dapp-runtime=none \
    -Demit-macos-app=false \
    -Demit-xcframework=true \
    -Dxcframework-target=native
)

rm -rf "$FRAMEWORK_DIR"
cp -R "$UPSTREAM_DIR/macos/GhosttyKit.xcframework" "$FRAMEWORK_DIR"

echo
echo "GhosttyKit is ready at:"
echo "  $FRAMEWORK_DIR"
echo
echo "Next:"
echo "  cd $ROOT_DIR"
echo "  swift run BossMacApp"
