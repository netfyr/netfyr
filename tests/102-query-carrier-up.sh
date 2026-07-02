#!/bin/bash
# 102-query-carrier-up.sh
# Integration test: Verify that query reports carrier=true and a correctly
# formatted MAC address for an ethernet interface whose peer is up.
# Mapped to spec acceptance criterion:
#   "Scenario: Query a specific interface by name
#    And the entity has the correct mtu, mac, and carrier values for eth0"
#
# When both ends of a veth pair are administratively up in the same namespace,
# each end reports carrier=true (the peer is connected and up). This test
# complements 102-query-link-down.sh which verifies carrier=false.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-carrier-up.sh
#   bash tests/102-query-carrier-up.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Create a veth pair; both ends are brought up by create_veth.
# When both ends are up, each end reports carrier=true (peer is present and up).
create_veth veth-carr0 veth-carr1

# Query veth-carr0 (both ends are up → carrier must be true).
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-carr0 \
    --output json)

# Assert: the interface appears in query output.
if ! echo "$output" | grep -q '"veth-carr0"'; then
    echo "FAIL: 102-query-carrier-up: interface 'veth-carr0' not found in output" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: carrier is true (both ends of the veth pair are up).
if ! echo "$output" | grep -q '"carrier": true'; then
    echo "FAIL: 102-query-carrier-up: expected carrier=true for up interface (both veth ends are up)" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "mac" field is present and formatted as aa:bb:cc:dd:ee:ff (lowercase hex).
# The spec says: mac | IFLA_ADDRESS from link message | 6-byte hardware address,
# formatted as aa:bb:cc:dd:ee:ff.
if ! echo "$output" | grep -qE '"mac": "[0-9a-f]{2}:[0-9a-f]{2}:[0-9a-f]{2}:[0-9a-f]{2}:[0-9a-f]{2}:[0-9a-f]{2}"'; then
    echo "FAIL: 102-query-carrier-up: 'mac' field missing or not in aa:bb:cc:dd:ee:ff format" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "type" field is "ethernet" (veth has ARPHRD_ETHER and no phy80211).
if ! echo "$output" | grep -q '"type": "ethernet"'; then
    echo "FAIL: 102-query-carrier-up: 'type' field missing or not 'ethernet'" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "enabled" field is true (both ends are administratively up).
if ! echo "$output" | grep -q '"enabled": true'; then
    echo "FAIL: 102-query-carrier-up: expected enabled=true for admin-up interface" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-carrier-up"
