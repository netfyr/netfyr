#!/bin/bash
# 102-query-addresses-ipv4-ipv6.sh
# Integration test: Query an ethernet interface and verify that IPv4 and IPv6
# addresses appear in their respective sub-objects ("ipv4" and "ipv6").
# Mapped to spec acceptance scenario:
#   "Then the entity's 'ipv4' sub-object contains the IPv4 address"
#   "And the entity's 'ipv6' sub-object contains the IPv6 address"
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-addresses-ipv4-ipv6.sh
#   bash tests/102-query-addresses-ipv4-ipv6.sh   (uses target/debug/netfyr fallback)

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

# Add IPv4 address first, then IPv6.
add_address veth-test0 10.99.5.1/24
add_address veth-test0 fd00:5::1/64

# Query the specific interface in daemon-free mode using JSON output.
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: "ipv4" sub-object is present in the output.
if ! echo "$output" | grep -q '"ipv4"'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain 'ipv4' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "ipv6" sub-object is present in the output.
if ! echo "$output" | grep -q '"ipv6"'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: IPv4 address is present somewhere in the output (inside ipv4 sub-object).
if ! echo "$output" | grep -q '10\.99\.5\.1/24'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain IPv4 address 10.99.5.1/24" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: IPv6 address is present somewhere in the output (inside ipv6 sub-object).
if ! echo "$output" | grep -q 'fd00:5::1/64'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain IPv6 address fd00:5::1/64" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: IPv6 address entry has a "dad_state" field.
if ! echo "$output" | grep -q '"dad_state"'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: IPv6 address entry does not contain 'dad_state' field" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-addresses-ipv4-ipv6"
