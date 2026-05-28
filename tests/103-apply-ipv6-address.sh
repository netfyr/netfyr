#!/bin/bash
# 103-apply-ipv6-address.sh
# Integration test: Apply a policy with an IPv6 address and verify it
# appears on the interface and in query output.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-ipv6-address: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "fd00:aa::1/64"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-ipv6-address: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ADDR_OUTPUT=$(ip -6 addr show dev veth-test0)
if ! echo "$ADDR_OUTPUT" | grep -q "fd00:aa::1/64"; then
    echo "FAIL: 103-apply-ipv6-address: address fd00:aa::1/64 not found" >&2
    echo "      ip -6 addr output: $ADDR_OUTPUT" >&2
    exit 1
fi

QUERY_OUTPUT=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

if ! echo "$QUERY_OUTPUT" | grep -q 'fd00:aa::1/64'; then
    echo "FAIL: 103-apply-ipv6-address: query output does not contain fd00:aa::1/64" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-ipv6-address"
