#!/usr/bin/env bash
# Workspace-status script for the Boss release build.
#
# Called by Bazel when --workspace_status_command is set (always, not just
# with --stamp). The STABLE_* keys go to stable-status.txt; all others go
# to volatile-status.txt.
#
# BUILD_EMBED_LABEL is a special Bazel key: its value is used by
# apple_bundle_version's build_label_pattern mechanism to stamp
# CFBundleShortVersionString in Boss.app's Info.plist.
set -euo pipefail

SHA=$(jj log --no-graph -r @ -T 'commit_id.short(7)' 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo "unknown")
BUILD_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Compute a semantic version string from git tags (boss-v* prefix).
# Release build (exact tag match): boss-v1.0.4 → "1.0.4"
# Dev build (commits past tag):    boss-v1.0.4-16-gf3be785 → "1.0.4-dev-<SHA>"
# Uses $SHA (from jj/git above) for the dev suffix so STABLE_BOSS_VERSION
# and STABLE_BOSS_GIT_SHA always contain the same commit identifier.
#
# In a jj workspace there is no .git directory — git describe must be pointed
# at the bare git store jj maintains at .jj/repo/store/git.  jj commit IDs
# are git SHAs, so we can pass the full commit_id directly to git describe.
# In a plain git checkout the standard `git describe` path is used instead.
if [ -d ".jj" ]; then
    # jj workspace: resolve tags via the bare git store.
    # Use the full jj commit ID (which is the git SHA) as the describe target.
    FULL_SHA=$(jj log --no-graph -r @ -T 'commit_id' 2>/dev/null || echo "")
    if [ -n "$FULL_SHA" ]; then
        export GIT_DIR=".jj/repo/store/git"
        DESCRIBE=$(git describe --tags --match "boss-v*" --abbrev=0 "$FULL_SHA" 2>/dev/null || echo "")
        DESCRIBE_EXACT=$(git describe --tags --match "boss-v*" --exact-match "$FULL_SHA" 2>/dev/null || echo "")
        unset GIT_DIR
    else
        DESCRIBE=""
        DESCRIBE_EXACT=""
    fi
else
    DESCRIBE=$(git describe --tags --match "boss-v*" --abbrev=0 2>/dev/null || echo "")
    DESCRIBE_EXACT=$(git describe --tags --match "boss-v*" --exact-match 2>/dev/null || echo "")
fi

if [ -z "$DESCRIBE" ]; then
    BOSS_VERSION="0.0.0-dev-${SHA}"
    BOSS_BASE_VERSION="0.0.0"
elif [ -n "$DESCRIBE_EXACT" ]; then
    # Exactly on a release tag: strip the "boss-v" prefix.
    BOSS_VERSION="${DESCRIBE#boss-v}"
    BOSS_BASE_VERSION="${BOSS_VERSION}"
else
    # Dev build: strip "boss-v" from the latest tag, append "-dev-<SHA>".
    BOSS_BASE_VERSION="${DESCRIBE#boss-v}"
    BOSS_VERSION="${BOSS_BASE_VERSION}-dev-${SHA}"
fi

# Goes to stable-status.txt — consumed by build_info_rs, boss_short_version_plist,
# and boss_pkg_unsigned to embed the SHA in the .pkg filename.
echo "STABLE_BOSS_VERSION $BOSS_VERSION"
echo "STABLE_BOSS_BASE_VERSION $BOSS_BASE_VERSION"
echo "STABLE_BOSS_GIT_SHA $SHA"
echo "STABLE_BOSS_BUILD_TIME $BUILD_TIME"

# Goes to volatile-status.txt — not used for version stamping but kept for
# build tooling compatibility.
echo "BUILD_EMBED_LABEL $BOSS_BASE_VERSION"
