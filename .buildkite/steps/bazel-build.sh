#!/usr/bin/env bash
# bazel-build.sh — bazel build //... (dependency-graph compile guard).
# Catches visibility violations, missing deps, and broken generated files.
# macOS-only Swift targets (//tools/boss/app-macos/...) and the macOS installer
# package (//tools/boss/installer/...) are excluded here; they run on the
# mac-app-build step on a macos-arm64 agent.
# //tools/boss/installer/... is excluded because boss_pkg_payload transitively
# depends on //tools/boss/app-macos:Boss (Swift), which has no Linux toolchain.
# //tools/boss/experiments/... is excluded for the same Swift-toolchain reason
# (e.g. textual-perf is a SwiftPM-only macOS app); experiments are local-run
# playgrounds and aren't built by mac-app-build either.
set -euo pipefail

echo "--- [bazel-build] starting"
echo "[bazel-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel build --verbose_failures --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/... -//tools/boss/experiments/...

echo "[bazel-build] ok"
