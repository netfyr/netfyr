#!/bin/bash
# 103-apply-ipv6-addr-gen-mode.sh
# Integration test: Apply a policy with ipv6.link_local=none and verify that
# /proc/sys/net/ipv6/conf/<iface>/addr_gen_mode is set to 1.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-ipv6-addr-gen-mode: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Bring veth-test0 down so addr_gen_mode can be changed before UP time.
ip link set veth-test0 down

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
ipv6:
  link_local: none
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-ipv6-addr-gen-mode: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ADDR_GEN_MODE=$(cat /proc/sys/net/ipv6/conf/veth-test0/addr_gen_mode 2>/dev/null)
if [[ "$ADDR_GEN_MODE" != "1" ]]; then
    echo "FAIL: 103-apply-ipv6-addr-gen-mode: expected addr_gen_mode=1, got '${ADDR_GEN_MODE}'" >&2
    exit 1
fi

echo "PASS: 103-apply-ipv6-addr-gen-mode"
