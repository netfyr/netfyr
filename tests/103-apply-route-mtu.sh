#!/bin/bash
# 103-apply-route-mtu.sh
# Integration test: Apply a policy with a route that has an mtu attribute
# and verify the kernel route shows the correct MTU.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-route-mtu: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

add_address veth-test0 10.99.0.1/24

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
addresses:
  - "10.99.0.1/24"
routes:
  - destination: "10.100.0.0/24"
    gateway: "10.99.0.2"
    mtu: 1400
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-route-mtu: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ROUTE_OUTPUT=$(ip route)
if ! echo "$ROUTE_OUTPUT" | grep "10.100.0.0/24" | grep -q "mtu 1400"; then
    echo "FAIL: 103-apply-route-mtu: route 10.100.0.0/24 does not have mtu 1400" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-route-mtu"
