#!/bin/bash
# 103-apply-add-route.sh
# Integration test: Add a static route via netfyr apply.
# Mapped to spec shell scenario: "Add a route in namespace".

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-add-route: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Pre-configure the address so that gateway 10.99.0.2 is reachable via the
# connected /24 subnet when the route is added.
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
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-add-route: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ROUTE_OUTPUT=$(ip route)
if ! echo "$ROUTE_OUTPUT" | grep -q "10.100.0.0/24 via 10.99.0.2"; then
    echo "FAIL: 103-apply-add-route: route 10.100.0.0/24 via 10.99.0.2 not found" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-add-route"
