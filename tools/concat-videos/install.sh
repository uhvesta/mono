#!/usr/bin/env bash
# install.sh — install the concat-videos shell tool and Finder Quick Action.
#
#  1. Copies ./concat-videos to ~/.local/bin/concat-videos and chmod +x.
#  2. Copies ./Concat Videos.workflow to ~/Library/Services/, rewriting the
#     Run Shell Script body to point at the installed binary.
#
# After installation, restart Finder (or log out/in) for the Quick Action to
# appear under right-click → Quick Actions on selected movie files.
#
# Usage: ./install.sh           # install
#        ./install.sh uninstall # remove both the binary and the Quick Action

set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="${HOME}/.local/bin"
bin_path="${bin_dir}/concat-videos"
services_dir="${HOME}/Library/Services"
workflow_name="Concat Videos.workflow"
workflow_dst="${services_dir}/${workflow_name}"

uninstall() {
  if [[ -e "$bin_path" ]]; then
    rm -f -- "$bin_path"
    echo "removed: $bin_path"
  fi
  if [[ -e "$workflow_dst" ]]; then
    rm -rf -- "$workflow_dst"
    echo "removed: $workflow_dst"
  fi
  /System/Library/CoreServices/pbs -update >/dev/null 2>&1 || true
  echo "done. You may need to restart Finder for the Quick Action to disappear from menus."
}

case "${1:-install}" in
  uninstall|remove)
    uninstall
    exit 0
    ;;
  install)
    ;;
  *)
    echo "usage: $0 [install|uninstall]" >&2
    exit 64
    ;;
esac

if ! command -v ffmpeg >/dev/null 2>&1 \
  && [[ ! -x /opt/homebrew/bin/ffmpeg ]] \
  && [[ ! -x /usr/local/bin/ffmpeg ]]; then
  echo "warning: ffmpeg not found on PATH or in /opt/homebrew/bin or /usr/local/bin." >&2
  echo "         The Quick Action will fail until you install it: brew install ffmpeg" >&2
fi

mkdir -p "$bin_dir"
install -m 0755 "$here/concat-videos" "$bin_path"
echo "installed: $bin_path"

mkdir -p "$services_dir"
rm -rf -- "$workflow_dst"
cp -R "$here/$workflow_name" "$workflow_dst"

# Rewrite the placeholder in the Run Shell Script step to point at the
# installed binary. sed in place needs an empty backup arg on macOS.
wflow="$workflow_dst/Contents/document.wflow"
escaped_path="${bin_path//\//\\/}"
/usr/bin/sed -i '' "s|__CONCAT_VIDEOS_BIN__|${escaped_path}|g" "$wflow"
echo "installed: $workflow_dst"

# Nudge the Services system to pick up the new workflow.
/System/Library/CoreServices/pbs -update >/dev/null 2>&1 || true

cat <<EOF

Installed Concat Videos Quick Action.

To use:
  1. Select two or more .mp4 files in Finder.
  2. Right-click → Quick Actions → Concat Videos.
     (Or → Services → Concat Videos on older macOS.)
  3. The merged file appears alongside the inputs.

If "Concat Videos" doesn't show in the menu, try:
  - killall Finder
  - System Settings → Privacy & Security → Extensions → Finder Extensions / Quick Actions
    and make sure "Concat Videos" is enabled.

Logs: ~/Library/Logs/concat-videos/concat-videos.log

Direct CLI usage:
  ${bin_path} FILE1 FILE2 [FILE3 ...]
EOF
