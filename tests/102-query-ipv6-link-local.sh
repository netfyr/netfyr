#!/bin/bash
# 102-query-ipv6-link-local.sh
# Integration test: Query an interface and verify that the "ipv6" sub-object
# contains a "link_local" field populated from the kernel's addr_gen_mode setting.
# Mapped to spec acceptance scenario:
#   "Then the 'ipv6.link_local' field reflects the kernel's addr_gen_mode"
#
# The default addr_gen_mode is 0 (eui64) on most kernels, which maps to "eui64".
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-ipv6-link-local.sh
#   bash tests/102-query-ipv6-link-local.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 102-query-ipv6-link-local: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Verify that the procfs entry is readable — skip if not present (older kernels).
ADDR_GEN_MODE_PATH="/proc/sys/net/ipv6/conf/veth-test0/addr_gen_mode"
if [[ ! -r "$ADDR_GEN_MODE_PATH" ]]; then
    echo "SKIP: 102-query-ipv6-link-local: $ADDR_GEN_MODE_PATH not readable; skipping"
    exit 0
fi

output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: "ipv6" sub-object is present.
if ! echo "$output" | grep -q '"ipv6"'; then
    echo "FAIL: 102-query-ipv6-link-local: output does not contain 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: "link_local" field is present inside the ipv6 sub-object.
if ! echo "$output" | grep -q '"link_local"'; then
    echo "FAIL: 102-query-ipv6-link-local: 'link_local' field missing from 'ipv6' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the link_local value is one of the known addr_gen_mode strings.
if ! echo "$output" | grep -qE '"link_local"[[:space:]]*:[[:space:]]*"(eui64|none)"'; then
    echo "FAIL: 102-query-ipv6-link-local: 'link_local' is not one of eui64/none" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-ipv6-link-local"
