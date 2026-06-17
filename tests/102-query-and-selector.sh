#!/bin/bash
# 102-query-and-selector.sh
# Integration test: Verify AND logic when multiple non-type selector fields are
# specified together. All specified fields must match the same interface.
# Mapped to spec acceptance criterion:
#   "Scenario: Query with multiple selector fields uses AND logic
#    Given ethernet interfaces 'eth0' (driver: ixgbe, mac: aa:bb:cc:dd:ee:01) and
#          'eth1' (driver: ixgbe, mac: aa:bb:cc:dd:ee:02)
#    When query is called with selector driver='ixgbe' AND mac='aa:bb:cc:dd:ee:01'
#    Then the result contains exactly one entity with name 'eth0'"
#
# Since driver sysfs is unavailable for veth interfaces, we test AND logic using
# name + mac selectors:
#   Case 1: name=veth-and0 AND mac=<MAC of veth-and0> -> only veth-and0 returned
#   Case 2: name=veth-and0 AND mac=<MAC of veth-and1> -> empty result
#           (name matches veth-and0 but mac matches veth-and1; no interface satisfies both)
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-and-selector.sh
#   bash tests/102-query-and-selector.sh   (uses target/debug/netfyr fallback)

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
create_veth veth-and0 veth-and1

# Capture the MAC addresses of both veth interfaces.
MAC0=$(ip link show dev veth-and0 | awk '/link\/ether/ {print $2}')
MAC1=$(ip link show dev veth-and1 | awk '/link\/ether/ {print $2}')

if [[ -z "$MAC0" || -z "$MAC1" ]]; then
    echo "FAIL: 102-query-and-selector: could not extract MAC addresses from ip link show" >&2
    exit 1
fi

# ── Case 1: name AND mac both match veth-and0 ──────────────────────────────────
# Both selector fields point to the same interface; only veth-and0 must appear.
output1=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector "name=veth-and0" \
    --selector "mac=$MAC0" \
    --output json)

if ! echo "$output1" | grep -q '"veth-and0"'; then
    echo "FAIL: 102-query-and-selector: case 1 - veth-and0 must appear when name and mac both match it" >&2
    echo "  name=veth-and0, mac=$MAC0" >&2
    echo "  Output: $output1" >&2
    exit 1
fi

if echo "$output1" | grep -q '"veth-and1"'; then
    echo "FAIL: 102-query-and-selector: case 1 - veth-and1 must not appear (AND logic: selector targets veth-and0 only)" >&2
    echo "  name=veth-and0, mac=$MAC0" >&2
    echo "  Output: $output1" >&2
    exit 1
fi

# ── Case 2: name matches veth-and0 but mac matches veth-and1 ───────────────────
# No interface satisfies both conditions simultaneously, so the result must be empty.
output2=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector "name=veth-and0" \
    --selector "mac=$MAC1" \
    --output json)

if echo "$output2" | grep -q '"veth-and0"'; then
    echo "FAIL: 102-query-and-selector: case 2 - veth-and0 must not appear (name matches but mac does not)" >&2
    echo "  name=veth-and0, mac=$MAC1 (veth-and1's MAC)" >&2
    echo "  Output: $output2" >&2
    exit 1
fi

if echo "$output2" | grep -q '"veth-and1"'; then
    echo "FAIL: 102-query-and-selector: case 2 - veth-and1 must not appear (mac matches but name does not)" >&2
    echo "  name=veth-and0, mac=$MAC1 (veth-and1's MAC)" >&2
    echo "  Output: $output2" >&2
    exit 1
fi

echo "PASS: 102-query-and-selector"
