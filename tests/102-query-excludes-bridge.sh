#!/bin/bash
# 102-query-excludes-bridge.sh
# Integration test: Verify that virtual aggregate interfaces (bridge, bond) are
# excluded from query results while a veth pair (non-virtual) is included.
#
# Mapped to spec acceptance criterion:
#   "Scenario: Query excludes virtual aggregate interfaces
#    Given interfaces 'eth0' (ethernet), 'br0' (bridge), 'bond0' (bond)
#    When query is called with no selector
#    Then bridge and bond interfaces are excluded"
#
# In an unprivileged user+net namespace we create:
#   - veth-inc0 / veth-inc1 (veth pair, should appear in output)
#   - br0 (bridge, must NOT appear in output)
#   - bond0 (bond, must NOT appear in output)
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-excludes-bridge.sh
#   bash tests/102-query-excludes-bridge.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Create a veth pair (should appear in query output as ethernet).
create_veth veth-inc0 veth-inc1

# Create a bridge interface (must be excluded from query output).
ip link add br0 type bridge
ip link set br0 up

# Create a bond interface (must be excluded from query output).
ip link add bond0 type bond
ip link set bond0 up

# Query all ethernet interfaces (no name selector) in daemon-free mode.
output=$("$NETFYR_BIN" query --selector type=ethernet --output json)

# Assert: veth pair ends appear (veth is non-virtual and must be included).
if ! echo "$output" | grep -q '"veth-inc0"'; then
    echo "FAIL: 102-query-excludes-bridge: veth-inc0 missing from output (must be included)" >&2
    echo "Output: $output" >&2
    exit 1
fi

if ! echo "$output" | grep -q '"veth-inc1"'; then
    echo "FAIL: 102-query-excludes-bridge: veth-inc1 missing from output (must be included)" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: bridge interface is NOT present in query output.
if echo "$output" | grep -q '"br0"'; then
    echo "FAIL: 102-query-excludes-bridge: bridge 'br0' appears in output but must be excluded" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: bond interface is NOT present in query output.
if echo "$output" | grep -q '"bond0"'; then
    echo "FAIL: 102-query-excludes-bridge: bond 'bond0' appears in output but must be excluded" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-excludes-bridge"
