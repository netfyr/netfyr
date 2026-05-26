#!/bin/bash
# 102-query-link-down.sh
# Integration test: Verify that an admin-down ethernet interface is still returned
# by query, with carrier=false, and that other fields (name, mtu, mac) are present.
# The speed field must be absent when no link speed is available.
#
# Mapped to spec acceptance criterion:
#   "Given an ethernet interface 'eth0' with carrier down and no link speed available
#    When query is called for 'eth0'
#    Then the entity has carrier=false
#    And the speed field is None (omitted)
#    And other fields (name, mtu, mac) are still present"
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-link-down.sh
#   bash tests/102-query-link-down.sh   (uses target/debug/netfyr fallback)

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

# Create a veth pair (both ends brought up by create_veth).
create_veth veth-down0 veth-down1

# Administratively bring veth-down0 down.
ip link set veth-down0 down

# Query the admin-down interface.
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-down0 \
    --output json)

# Assert: the interface still appears in query output (graceful handling).
if ! echo "$output" | grep -q '"veth-down0"'; then
    echo "FAIL: 102-query-link-down: admin-down interface 'veth-down0' not present in output" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: carrier is false (interface is admin-down, so no carrier signal).
if ! echo "$output" | grep -q '"carrier": false'; then
    echo "FAIL: 102-query-link-down: expected carrier=false for admin-down interface" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "name" field is present.
if ! echo "$output" | grep -q '"name"'; then
    echo "FAIL: 102-query-link-down: 'name' field missing from output for down interface" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "mtu" field is present.
if ! echo "$output" | grep -q '"mtu"'; then
    echo "FAIL: 102-query-link-down: 'mtu' field missing from output for down interface" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "mac" field is present.
if ! echo "$output" | grep -q '"mac"'; then
    echo "FAIL: 102-query-link-down: 'mac' field missing from output for down interface" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "speed" field is absent — veth interfaces have no speed file in sysfs,
# and the spec says the speed field should be omitted (None) when unavailable.
if echo "$output" | grep -q '"speed"'; then
    echo "FAIL: 102-query-link-down: 'speed' field present in output but should be omitted for veth" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-link-down"
