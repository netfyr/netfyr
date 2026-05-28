#!/bin/bash
# 102-query-all-veth-pair.sh
# Integration test: Query all ethernet interfaces and verify both ends of a veth
# pair appear in the results.
# Mapped to spec acceptance scenario: "Query returns both ends of a veth pair".
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-all-veth-pair.sh
#   bash tests/102-query-all-veth-pair.sh   (uses target/debug/netfyr fallback)

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
create_veth veth-a veth-b

# Query all ethernet interfaces (no name selector) in daemon-free mode.
output=$("$NETFYR_BIN" query --selector type=ethernet --output json)

# Assert: both ends of the veth pair are present.
if ! echo "$output" | grep -q '"veth-a"'; then
    echo "FAIL: 102-query-all-veth-pair: output does not contain 'veth-a'" >&2
    echo "Output: $output" >&2
    exit 1
fi

if ! echo "$output" | grep -q '"veth-b"'; then
    echo "FAIL: 102-query-all-veth-pair: output does not contain 'veth-b'" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "type" field is "ethernet" (veth has ARPHRD_ETHER and no phy80211).
if ! echo "$output" | grep -q '"type": "ethernet"'; then
    echo "FAIL: 102-query-all-veth-pair: 'type' field missing or not 'ethernet'" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-all-veth-pair"
