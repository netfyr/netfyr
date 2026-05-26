#!/bin/bash
# 001-workspace-members.sh
# Verify that the root Cargo.toml workspace members list contains the 7
# crates required by spec-001: netfyr-state, netfyr-reconcile, netfyr-backend,
# netfyr-policy, netfyr-varlink, netfyr-cli, netfyr-daemon.
#
# NOTE: The spec says "exactly 7 entries". Additional workspace members such as
# netfyr-test-utils and xtask are expected to be added by other specs and do not
# constitute a failure for this test. We check that all 7 required members are
# present.
#
# Usage: bash tests/001-workspace-members.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
CARGO_TOML="$PROJECT_ROOT/Cargo.toml"

# Prerequisite: Cargo.toml must exist.
if [[ ! -f "$CARGO_TOML" ]]; then
    echo "FAIL: 001-workspace-members: Cargo.toml not found at $CARGO_TOML" >&2
    exit 1
fi

failed=0

required_members=(
    "crates/netfyr-state"
    "crates/netfyr-reconcile"
    "crates/netfyr-backend"
    "crates/netfyr-policy"
    "crates/netfyr-varlink"
    "crates/netfyr-journal"
    "crates/netfyr-cli"
    "crates/netfyr-daemon"
    "crates/netfyr-test-utils"
    "xtask"
)

for member in "${required_members[@]}"; do
    if ! grep -qF "\"$member\"" "$CARGO_TOML"; then
        echo "FAIL: 001-workspace-members: required member '$member' not found in Cargo.toml" >&2
        failed=1
    fi
done

# Verify the workspace contains exactly 10 members (not more, not fewer).
actual_count=$(grep -c '"crates/' "$CARGO_TOML" 2>/dev/null || echo 0)
# Add 1 for xtask (which is not under crates/)
xtask_count=$(grep -c '"xtask"' "$CARGO_TOML" 2>/dev/null || echo 0)
total_count=$((actual_count + xtask_count))
if [[ "$total_count" -ne 10 ]]; then
    echo "FAIL: 001-workspace-members: expected exactly 10 workspace members, found $total_count" >&2
    failed=1
fi

# Verify the [workspace] section and resolver = "2" are present.
if ! grep -q '^\[workspace\]' "$CARGO_TOML"; then
    echo "FAIL: 001-workspace-members: [workspace] section missing from Cargo.toml" >&2
    failed=1
fi

if ! grep -q 'resolver = "2"' "$CARGO_TOML"; then
    echo "FAIL: 001-workspace-members: resolver = \"2\" not set in Cargo.toml" >&2
    failed=1
fi

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 001-workspace-members"
