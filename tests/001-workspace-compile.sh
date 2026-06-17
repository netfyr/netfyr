#!/bin/bash
# 001-workspace-compile.sh
# Verify that the full Rust workspace compiled successfully by checking that
# the expected binary artifacts are present in target/debug/.
#
# The Makefile's integration-test target runs "cargo build" before executing
# any test scripts, so by the time this script runs the workspace has already
# been built. If cargo build failed, the Makefile would have exited early and
# this script would never be reached. We verify artifacts here to satisfy the
# acceptance criterion "all 10 crates are compiled" explicitly.
#
# Checked artifacts:
#   - target/debug/netfyr        (CLI binary from netfyr-cli package)
#   - target/debug/netfyr-cli    (stub binary from netfyr-cli package)
#   - target/debug/netfyr-daemon (daemon binary)
#   - target/debug/xtask         (xtask binary)
#
# Library crate artifacts (.rlib) are not checked individually because their
# filenames contain a hash; their compilation is guaranteed transitively by
# the CLI and daemon builds, which depend on all library crates.
#
# Usage: bash tests/001-workspace-compile.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
TARGET_DEBUG="$PROJECT_ROOT/target/debug"

# Prerequisite: target/debug directory must exist — implies cargo build ran.
if [[ ! -d "$TARGET_DEBUG" ]]; then
    echo "FAIL: 001-workspace-compile: target/debug/ not found; run 'cargo build' first" >&2
    exit 1
fi

failed=0

# All binary crates required by spec-001.
required_binaries=(
    "netfyr"
    "netfyr-cli"
    "netfyr-daemon"
    "xtask"
)

for bin in "${required_binaries[@]}"; do
    bin_path="$TARGET_DEBUG/$bin"
    if [[ ! -f "$bin_path" ]]; then
        echo "FAIL: 001-workspace-compile: binary '$bin' not found at $bin_path" >&2
        failed=1
    elif [[ ! -x "$bin_path" ]]; then
        echo "FAIL: 001-workspace-compile: binary '$bin' exists but is not executable" >&2
        failed=1
    fi
done

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 001-workspace-compile"
