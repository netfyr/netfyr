#!/bin/bash
# 103-apply-field-ordering.sh
# Integration test: Field changes within an entity are applied in correct order.
# Spec scenario: "Field changes within an entity are applied in correct order"
#
# A single apply sets mtu=9000, adds address "10.99.0.1/24", and adds a static
# route "10.100.0.0/24 via 10.99.0.2". If ordering were wrong (route before
# address), the kernel would reject the route with ENETUNREACH because the
# gateway 10.99.0.2 is only reachable via the connected /24 subnet.
# All three operations must succeed, proving: mtu → address → route ordering.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-field-ordering: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
mtu: 9000
addresses:
  - "10.99.0.1/24"
routes:
  - destination: "10.100.0.0/24"
    gateway: "10.99.0.2"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-field-ordering: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

# Verify mtu was set.
assert_mtu veth-test0 9000

# Verify address was added.
assert_has_address veth-test0 "10.99.0.1/24"

# Verify route was added.
ROUTE_OUTPUT=$(ip route)
if ! echo "$ROUTE_OUTPUT" | grep -q "10.100.0.0/24 via 10.99.0.2"; then
    echo "FAIL: 103-apply-field-ordering: route 10.100.0.0/24 via 10.99.0.2 not found" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-field-ordering"
