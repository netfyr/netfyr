#!/bin/bash
# 103-apply-two-default-routes.sh
# Integration test: Two interfaces can each have a default route with the same
# metric but different gateways. The second route must not be rejected with
# EEXIST — both routes must coexist in the kernel routing table.
#
# Requires: unshare, ip (iproute2)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-two-default-routes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

# Two veth pairs on different subnets.
create_veth veth-rt0 veth-rp0
create_veth veth-rt1 veth-rp1
add_address veth-rt0 10.99.90.1/24
add_address veth-rt1 10.99.91.1/24

# Apply a default route via each interface (same metric, different gateways).
POLICY0=$(mktemp --suffix=.yaml)
cat > "$POLICY0" <<'EOF'
selector:
  name: veth-rt0
addresses:
  - "10.99.90.1/24"
routes:
  - destination: "0.0.0.0/0"
    gateway: "10.99.90.254"
    metric: 100
EOF

POLICY1=$(mktemp --suffix=.yaml)
cat > "$POLICY1" <<'EOF'
selector:
  name: veth-rt1
addresses:
  - "10.99.91.1/24"
routes:
  - destination: "0.0.0.0/0"
    gateway: "10.99.91.254"
    metric: 100
EOF

"$NETFYR_BIN" apply "$POLICY0"
APPLY0_EXIT=$?
if [[ $APPLY0_EXIT -ne 0 ]]; then
    echo "FAIL: 103-apply-two-default-routes: first apply exited with code $APPLY0_EXIT" >&2
    exit 1
fi

"$NETFYR_BIN" apply "$POLICY1"
APPLY1_EXIT=$?
if [[ $APPLY1_EXIT -ne 0 ]]; then
    echo "FAIL: 103-apply-two-default-routes: second apply exited with code $APPLY1_EXIT" >&2
    exit 1
fi

# Both default routes must exist in the kernel routing table.
ROUTE_OUTPUT=$(ip route)

if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.90.254"; then
    echo "FAIL: 103-apply-two-default-routes: default route via 10.99.90.254 not found" >&2
    echo "      ip route output:" >&2
    echo "$ROUTE_OUTPUT" >&2
    exit 1
fi

if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.91.254"; then
    echo "FAIL: 103-apply-two-default-routes: default route via 10.99.91.254 not found" >&2
    echo "      ip route output:" >&2
    echo "$ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-two-default-routes"
