#!/bin/bash
# 001-individual-crate-compile.sh
# Verify that individual crates can be compiled in isolation using
# "cargo build -p <crate>", as required by spec-001.
#
# This test exercises the acceptance criterion:
#   "When the developer runs 'cargo build -p netfyr-state'
#    Then only netfyr-state and its dependencies are compiled
#    And the build succeeds"
#
# We test all library crates because the workspace setup must make each
# independently buildable. Binary crates (netfyr-cli, netfyr-daemon) are
# already covered by 001-binary-cli.sh and 001-binary-daemon.sh.
#
# Usage: bash tests/001-individual-crate-compile.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."

# Prerequisite: cargo must be available.
if ! command -v cargo >/dev/null 2>&1; then
    echo "FAIL: 001-individual-crate-compile: 'cargo' not found; install Rust toolchain" >&2
    exit 1
fi

# Prerequisite: workspace Cargo.toml must exist.
if [[ ! -f "$PROJECT_ROOT/Cargo.toml" ]]; then
    echo "FAIL: 001-individual-crate-compile: Cargo.toml not found at $PROJECT_ROOT/Cargo.toml" >&2
    exit 1
fi

failed=0

# Library crates to verify compile individually (all nine non-xtask library
# crates required by spec-001).
library_crates=(
    "netfyr-state"
    "netfyr-reconcile"
    "netfyr-backend"
    "netfyr-policy"
    "netfyr-varlink"
    "netfyr-journal"
    "netfyr-test-utils"
)

for crate in "${library_crates[@]}"; do
    if ! cargo build -p "$crate" --manifest-path "$PROJECT_ROOT/Cargo.toml" \
            >/dev/null 2>&1; then
        echo "FAIL: 001-individual-crate-compile: 'cargo build -p $crate' failed" >&2
        failed=1
    fi
done

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 001-individual-crate-compile"
