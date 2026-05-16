#!/usr/bin/env bash
# Guard: engine source must never call `gh pr merge`.
# Branch protection (GitHub) is the merge gate, not engine code.
# If this fires, any would-be auto-merge path must honour required-status-checks
# via the protection layer rather than bypassing it.
set -euo pipefail

src_dir="$TEST_SRCDIR/mono/tools/boss/engine/src"

if grep -r "gh pr merge" "$src_dir" --include="*.rs"; then
  echo "ERROR: 'gh pr merge' found in tools/boss/engine/src." >&2
  echo "The engine must not auto-merge PRs — branch protection is the gate." >&2
  exit 1
fi

echo "OK: no 'gh pr merge' calls in tools/boss/engine/src."
