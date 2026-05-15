#!/usr/bin/env bash
# pnpm-typecheck.sh — pnpm -r typecheck (TypeScript type checking across workspaces).
set -euo pipefail

echo "--- [pnpm-typecheck] starting"
echo "[pnpm-typecheck] pnpm: $(pnpm --version)"

pnpm -r typecheck

echo "[pnpm-typecheck] ok"
