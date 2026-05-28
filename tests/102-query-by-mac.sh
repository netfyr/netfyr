#!/bin/bash
# 102-query-by-mac.sh
# Integration test: Query an ethernet interface by MAC address selector, verifying
# that exactly the matching interface is returned and the other veth end is excluded.
# Mapped to spec acceptance scenario: "Query by MAC address in namespace".
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-by-mac.sh
#   bash tests/102-query-by-mac.sh   (uses target/debug/netfyr fallback)

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

# Create a veth pair; both ends are brought up by create_veth.
create_veth veth-test0 veth-test1

# Capture the MAC address of veth-test0 from `ip link show`.
# The kernel assigns random MACs to veth devices; we read the assigned value.
# Format of relevant line: "    link/ether aa:bb:cc:dd:ee:ff brd ff:ff:ff:ff:ff:ff"
MAC=$(ip link show dev veth-test0 | awk '/link\/ether/ {print $2}')

if [[ -z "$MAC" ]]; then
    echo "FAIL: 102-query-by-mac: could not extract MAC address from 'ip link show dev veth-test0'" >&2
    exit 1
fi

# Query by MAC address selector; only veth-test0 should appear.
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector "mac=$MAC" \
    --output json)

# Assert: the matching interface (veth-test0) is present.
if ! echo "$output" | grep -q '"veth-test0"'; then
    echo "FAIL: 102-query-by-mac: output does not contain 'veth-test0' (mac=$MAC)" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the non-matching interface (veth-test1) is absent.
if echo "$output" | grep -q '"veth-test1"'; then
    echo "FAIL: 102-query-by-mac: output contains 'veth-test1' but should not (filtered by mac=$MAC)" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "type" field is "ethernet" (veth has ARPHRD_ETHER and no phy80211).
if ! echo "$output" | grep -q '"type": "ethernet"'; then
    echo "FAIL: 102-query-by-mac: 'type' field missing or not 'ethernet'" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-by-mac"
