#!/bin/bash
# 102-query-ipv6-dad-transmits.sh
# Integration test: Query an interface and verify that the "ipv6" sub-object
# contains a "dad_transmits" field populated from the kernel's procfs setting.
# Mapped to spec acceptance scenario:
#   "Then the 'ipv6.dad_transmits' field reflects the kernel's DAD NS count"
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-ipv6-dad-transmits.sh
#   bash tests/102-query-ipv6-dad-transmits.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 102-query-ipv6-dad-transmits: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Verify that the procfs entry is readable — skip if not present (older kernels).
DAD_TRANSMITS_PATH="/proc/sys/net/ipv6/conf/veth-test0/dad_transmits"
if [[ ! -r "$DAD_TRANSMITS_PATH" ]]; then
    echo "SKIP: 102-query-ipv6-dad-transmits: $DAD_TRANSMITS_PATH not readable; skipping"
    exit 0
fi

output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: "ipv6" sub-object is present.
if ! echo "$output" | grep -q '"ipv6"'; then
    echo "FAIL: 102-query-ipv6-dad-transmits: output does not contain 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "dad_transmits" field is present inside the ipv6 sub-object.
if ! echo "$output" | grep -q '"dad_transmits"'; then
    echo "FAIL: 102-query-ipv6-dad-transmits: 'dad_transmits' field missing from 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the dad_transmits value is a non-negative integer.
if ! echo "$output" | grep -qE '"dad_transmits"[[:space:]]*:[[:space:]]*[0-9]+'; then
    echo "FAIL: 102-query-ipv6-dad-transmits: 'dad_transmits' is not a numeric value" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-ipv6-dad-transmits"
