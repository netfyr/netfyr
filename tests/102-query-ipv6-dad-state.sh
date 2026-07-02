#!/bin/bash
# 102-query-ipv6-dad-state.sh
# Integration test: Query an interface with an IPv6 address and verify that the
# address entry in the "ipv6" sub-object includes a "dad_state" field.
# Mapped to spec acceptance scenario:
#   "Then each IPv6 address entry in 'ipv6.addresses' has a 'dad_state' field"
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-ipv6-dad-state.sh
#   bash tests/102-query-ipv6-dad-state.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 102-query-ipv6-dad-state: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1
add_address veth-test0 fd00:10::1/64

output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: "ipv6" sub-object is present.
if ! echo "$output" | grep -q '"ipv6"'; then
    echo "FAIL: 102-query-ipv6-dad-state: output does not contain 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the IPv6 address appears in the output.
if ! echo "$output" | grep -q 'fd00:10::1/64'; then
    echo "FAIL: 102-query-ipv6-dad-state: output does not contain fd00:10::1/64" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: each IPv6 address entry has a "dad_state" field.
if ! echo "$output" | grep -q '"dad_state"'; then
    echo "FAIL: 102-query-ipv6-dad-state: IPv6 address entry does not contain 'dad_state' field" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the dad_state value is one of the four valid strings.
if ! echo "$output" | grep -qE '"dad_state"[[:space:]]*:[[:space:]]*"(preferred|tentative|dadfailed|deprecated)"'; then
    echo "FAIL: 102-query-ipv6-dad-state: 'dad_state' is not one of preferred/tentative/dadfailed/deprecated" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-ipv6-dad-state"
