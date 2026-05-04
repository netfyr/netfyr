#!/bin/bash
# 302-query-veth-by-type.sh
# Integration test: Query interfaces using the type=ethernet selector and
# verify that matching veth interfaces are returned.
# Reproduces bug 002: type=ethernet selector returned an empty list because
# the discovered selector did not carry entity_type.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/302-query-veth-by-type.sh
#   bash tests/302-query-veth-by-type.sh   (uses target/debug/netfyr fallback)

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
create_veth veth-t0 veth-t1

# ── Test 1: type=ethernet returns both veths ──────────────────────────────────

output=$("$NETFYR_BIN" query -s type=ethernet -o json)

if ! echo "$output" | grep -q '"veth-t0"'; then
    echo "FAIL: 302-query-veth-by-type: type=ethernet output does not contain 'veth-t0'" >&2
    echo "Output: $output" >&2
    exit 1
fi

if ! echo "$output" | grep -q '"veth-t1"'; then
    echo "FAIL: 302-query-veth-by-type: type=ethernet output does not contain 'veth-t1'" >&2
    echo "Output: $output" >&2
    exit 1
fi

# ── Test 2: name + type combined selector returns only the named interface ────

output=$("$NETFYR_BIN" query -s name=veth-t0 -s type=ethernet -o json)

if ! echo "$output" | grep -q '"veth-t0"'; then
    echo "FAIL: 302-query-veth-by-type: name+type output does not contain 'veth-t0'" >&2
    echo "Output: $output" >&2
    exit 1
fi

if echo "$output" | grep -q '"veth-t1"'; then
    echo "FAIL: 302-query-veth-by-type: name+type output should not contain 'veth-t1'" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 302-query-veth-by-type"
