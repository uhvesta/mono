#!/usr/bin/env bash
# pnpm-test.sh — pnpm -r test (JavaScript/TypeScript test suite).
# Advisory step: promoted to required once flake rate is visibly < 1%.
set -euo pipefail

echo "--- [pnpm-test] starting"
echo "[pnpm-test] pnpm: $(pnpm --version)"

pnpm -r test

echo "[pnpm-test] ok"
