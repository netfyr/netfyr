#!/bin/bash
# 102-query-addresses-ipv4-ipv6.sh
# Integration test: Query an ethernet interface and verify that both IPv4 and
# IPv6 addresses appear in the "addresses" field, in addition-order (IPv4 first,
# then IPv6), matching the spec acceptance criterion:
#   "Then the entity's 'addresses' field contains ["10.0.1.50/24", "fe80::1/64"]
#    And the address order matches the order they were added to the kernel"
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

# Add IPv4 address first, then IPv6 — order matters for the address-order check.
add_address veth-test0 10.99.5.1/24
add_address veth-test0 fd00:5::1/64

# Query the specific interface in daemon-free mode using JSON output.
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: "addresses" key is present in the output.
if ! echo "$output" | grep -q '"addresses"'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain 'addresses' field" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: IPv4 address is present in the addresses list.
if ! echo "$output" | grep -q '10\.99\.5\.1/24'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain IPv4 address 10.99.5.1/24" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: IPv6 address is present in the addresses list.
if ! echo "$output" | grep -q 'fd00:5::1/64'; then
    echo "FAIL: 102-query-addresses-ipv4-ipv6: output does not contain IPv6 address fd00:5::1/64" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: address order — IPv4 must appear before IPv6 in the output.
# We added IPv4 first; the spec requires the kernel addition order to be preserved.
assert_json_address_order "$output" "10.99.5.1/24" "fd00:5::1/64"

echo "PASS: 102-query-addresses-ipv4-ipv6"
