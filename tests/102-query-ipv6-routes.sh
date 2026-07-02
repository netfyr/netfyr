#!/bin/bash
# 102-query-ipv6-routes.sh
# Integration test: Query an interface with an IPv6 route and verify
# the query output includes the IPv6 route in the "ipv6" sub-object.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 102-query-ipv6-routes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1
add_address veth-test0 fd00:aa::1/64

# Add an explicit IPv6 route so there is something to query.
ip -6 route add fd00:bb::/64 via fd00:aa::2 dev veth-test0

QUERY_OUTPUT=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

if ! echo "$QUERY_OUTPUT" | grep -q '"ipv6"'; then
    echo "FAIL: 102-query-ipv6-routes: output does not contain 'ipv6' sub-object" >&2
    echo "      Output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q 'fd00:bb::'; then
    echo "FAIL: 102-query-ipv6-routes: output does not contain the IPv6 route fd00:bb::/64" >&2
    echo "      Output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"destination"'; then
    echo "FAIL: 102-query-ipv6-routes: route entries do not contain 'destination' field" >&2
    echo "      Output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 102-query-ipv6-routes"
