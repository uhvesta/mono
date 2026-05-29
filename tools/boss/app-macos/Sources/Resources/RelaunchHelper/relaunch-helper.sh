#!/bin/sh
# relaunch-helper.sh — relaunch Boss into a freshly-swapped bundle and watchdog its
# first launch.
#
# The bundle swap itself (current bundle -> .bak, staged bundle -> install location)
# is performed *in-process* by UpdateCore's `UpdateInstaller.applySwap` before this
# helper is spawned. This script handles only the parts that cannot run inside the
# Boss process once it has exited:
#
#   1. wait for the old Boss PID to exit (clean cold relaunch),
#   2. `open` the new bundle,
#   3. watchdog its first launch by polling for the first-launch-OK flag the new
#      version writes on `applicationDidFinishLaunching`,
#   4. on watchdog timeout, restore the .bak, reopen the previous version, and record
#      a rolled-back marker the app reads on its next launch to blocklist the bad
#      version.
#
# See tools/boss/docs/designs/automatic-boss-updates.md §4 (install / swap mechanics).
# Invoked as `/bin/sh relaunch-helper.sh --pid N --install P --backup P --flag P \
#   --rolled-back-marker P --version V --watchdog SECS` (no +x bit required).

set -u

pid=""
install=""
backup=""
flag=""
marker=""
version=""
watchdog=30

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) pid="$2"; shift 2 ;;
    --install) install="$2"; shift 2 ;;
    --backup) backup="$2"; shift 2 ;;
    --flag) flag="$2"; shift 2 ;;
    --rolled-back-marker) marker="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --watchdog) watchdog="$2"; shift 2 ;;
    *) shift ;;
  esac
done

[ -n "$install" ] || exit 64  # nothing to relaunch

# 1. Wait for the old Boss process to exit so the relaunch is a clean cold start
#    rather than just re-activating the still-running old instance. Capped at ~60s
#    so a wedged parent never strands this helper forever.
if [ -n "$pid" ]; then
  i=0
  while kill -0 "$pid" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -ge 600 ] && break
    sleep 0.1
  done
fi

# 2. Clear any stale success flag from a prior launch of this version, then relaunch.
[ -n "$flag" ] && rm -f "$flag"
open "$install"

# 3. Watchdog: poll for the first-launch-OK flag the new version writes once healthy.
ok=0
i=0
ticks=$((watchdog * 10))
while [ "$i" -lt "$ticks" ]; do
  if [ -n "$flag" ] && [ -f "$flag" ]; then
    ok=1
    break
  fi
  i=$((i + 1))
  sleep 0.1
done

if [ "$ok" -eq 1 ]; then
  # Clean first launch — the new version is committed; drop the backup.
  [ -n "$backup" ] && rm -rf "$backup"
  exit 0
fi

# 4. Watchdog timed out — the new version never reported a healthy launch. Restore
#    the previous bundle and reopen it, then record the failed version so the app
#    blocklists it on next launch and never re-attempts this update.
if [ -n "$backup" ] && [ -d "$backup" ]; then
  rm -rf "$install"
  mv "$backup" "$install"
fi
if [ -n "$marker" ] && [ -n "$version" ]; then
  printf '%s\n' "$version" > "$marker"
fi
open "$install"
exit 1
