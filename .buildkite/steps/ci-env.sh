#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

BAZEL_STARTUP_FLAGS=""

STARTUP_RC=".ci.${OS_TYPE}.startup.bazelrc"
if [[ -f "$STARTUP_RC" ]]; then
  BAZEL_STARTUP_FLAGS="--bazelrc=$STARTUP_RC"
fi

export REPOBIN_BAZEL_STARTUP_FLAGS="$BAZEL_STARTUP_FLAGS"

# Wrap bazel and pass in ci configuration
bazel() {
  local subcommand="$1"
  shift

  command bazel \
    $BAZEL_STARTUP_FLAGS \
    "$subcommand" \
    --config="ci-${OS_TYPE}" \
    "$@"
}
