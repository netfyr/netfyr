#!/bin/bash
# 103-apply-route-table.sh
# Integration test: Apply a policy with a route in a non-default routing
# table and verify it appears in that table.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-route-table: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

add_address veth-test0 10.99.0.1/24

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.0.1/24"
routes:
  - destination: "10.100.0.0/24"
    gateway: "10.99.0.2"
    table: 100
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-route-table: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ROUTE_OUTPUT=$(ip route show table 100)
if ! echo "$ROUTE_OUTPUT" | grep -q "10.100.0.0/24 via 10.99.0.2"; then
    echo "FAIL: 103-apply-route-table: route 10.100.0.0/24 not found in table 100" >&2
    echo "      ip route show table 100: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-route-table"
