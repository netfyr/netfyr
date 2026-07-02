#!/bin/bash
# 103-apply-ipv6-dad-transmits.sh
# Integration test: Apply a policy with ipv6.dad_transmits=3 and verify that
# /proc/sys/net/ipv6/conf/<iface>/dad_transmits is set to 3.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-ipv6-dad-transmits: netfyr binary not found at $NETFYR_BIN" >&2
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
ipv6:
  dad_transmits: 3
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-ipv6-dad-transmits: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

DAD_TRANSMITS=$(cat /proc/sys/net/ipv6/conf/veth-test0/dad_transmits 2>/dev/null)
if [[ "$DAD_TRANSMITS" != "3" ]]; then
    echo "FAIL: 103-apply-ipv6-dad-transmits: expected dad_transmits=3, got '${DAD_TRANSMITS}'" >&2
    exit 1
fi

echo "PASS: 103-apply-ipv6-dad-transmits"
