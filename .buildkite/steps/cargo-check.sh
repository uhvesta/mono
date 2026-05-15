#!/usr/bin/env bash
# cargo-check.sh — cargo check --workspace (fast compile guard).
# Runs even when bazel target graph is broken (e.g., missing srcs entry).
set -euo pipefail

echo "--- [cargo-check] starting"
echo "[cargo-check] rustc: $(rustc --version)"
echo "[cargo-check] cargo: $(cargo --version)"

cargo check --workspace

echo "[cargo-check] ok"
