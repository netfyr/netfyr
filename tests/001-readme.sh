#!/bin/bash
# 001-readme.sh
# Verify that README.md exists and covers the key topics required by spec-001:
#   - Describes what netfyr does
#   - Lists the seven crates with their roles
#   - Includes usage examples for apply and query commands
#   - Includes build and test instructions
#
# Usage: bash tests/001-readme.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
README="$PROJECT_ROOT/README.md"

# Prerequisite: README.md must exist.
if [[ ! -f "$README" ]]; then
    echo "FAIL: 001-readme: README.md not found at $README" >&2
    exit 1
fi

failed=0

# Must describe what netfyr does (project summary paragraph).
if ! grep -qi 'declarative\|network configuration\|netlink' "$README"; then
    echo "FAIL: 001-readme: README.md does not describe what netfyr does (expected terms: declarative, network configuration, netlink)" >&2
    failed=1
fi

# Must list all ten crates with their roles.
required_crates=(
    "netfyr-state"
    "netfyr-reconcile"
    "netfyr-backend"
    "netfyr-policy"
    "netfyr-varlink"
    "netfyr-journal"
    "netfyr-cli"
    "netfyr-daemon"
    "netfyr-test-utils"
    "xtask"
)

for crate in "${required_crates[@]}"; do
    if ! grep -q "$crate" "$README"; then
        echo "FAIL: 001-readme: README.md does not mention crate '$crate'" >&2
        failed=1
    fi
done

# Must include a usage example for the apply command.
if ! grep -q 'netfyr apply' "$README"; then
    echo "FAIL: 001-readme: README.md does not include a usage example for 'netfyr apply'" >&2
    failed=1
fi

# Must include a usage example for the query command.
if ! grep -q 'netfyr query' "$README"; then
    echo "FAIL: 001-readme: README.md does not include a usage example for 'netfyr query'" >&2
    failed=1
fi

# Must include build instructions (cargo build).
if ! grep -q 'cargo build' "$README"; then
    echo "FAIL: 001-readme: README.md does not include build instructions ('cargo build')" >&2
    failed=1
fi

# Must include test instructions (cargo test and/or make integration-test).
if ! grep -q 'cargo test' "$README"; then
    echo "FAIL: 001-readme: README.md does not include test instructions ('cargo test')" >&2
    failed=1
fi

if ! grep -q 'integration-test' "$README"; then
    echo "FAIL: 001-readme: README.md does not mention 'make integration-test'" >&2
    failed=1
fi

# Must reference the LICENSE file.
if ! grep -qi 'license' "$README"; then
    echo "FAIL: 001-readme: README.md does not reference the LICENSE file" >&2
    failed=1
fi

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 001-readme"
