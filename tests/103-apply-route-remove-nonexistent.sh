#!/bin/bash
# 103-apply-route-remove-nonexistent.sh
# Integration test: Removing a route that is absent from the kernel counts as
# success — the apply exits 0 and is_success() returns true.
# Spec AC-12: "Removing a non-existent route counts as success"
#
# When find_route_message returns None for a route in the to-remove list,
# apply_modify_fields pushes to fields_changed (succeeded), not to failed or
# skipped. The unit test for find_route_message covers the specific code path.
# This shell test verifies the end-to-end property across two sub-cases:
#   1. Apply "no routes" policy when interface already has no routes → exit 0.
#   2. Add a route, externally delete it, apply policy without it → the diff
#      sees no live route to remove, so the apply is a no-op → exit 0.
#
# Requires: unshare, ip (iproute2)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-route-remove-nonexistent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent
netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1
add_address veth-test0 10.99.88.1/24

POLICY_NO_ROUTES=$(mktemp --suffix=.yaml)
cat > "$POLICY_NO_ROUTES" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.88.1/24"
routes: []
EOF

# --- Sub-case 1: "no routes" policy applied when interface has no static routes ---
#
# The interface has a kernel prefix route (10.99.88.0/24, proto kernel) which is
# never removed. The desired route list is empty and there are no user routes to
# remove → apply exits 0.
"$NETFYR_BIN" apply "$POLICY_NO_ROUTES"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-route-remove-nonexistent: sub-case 1: no-route apply exited $EXIT_CODE" >&2
    exit 1
fi

# --- Sub-case 2: add a route, delete it externally, apply policy without it ---
#
# The route is added via apply, then removed externally (simulating the kernel or
# an external tool removing it before the next apply). When the policy without the
# route is applied, the live state has no such route, so the diff produces nothing
# to remove → apply exits 0 (success, as AC-12 requires).
POLICY_WITH_ROUTE=$(mktemp --suffix=.yaml)
cat > "$POLICY_WITH_ROUTE" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.88.1/24"
routes:
  - destination: "10.100.0.0/24"
    gateway: "10.99.88.254"
EOF

"$NETFYR_BIN" apply "$POLICY_WITH_ROUTE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-route-remove-nonexistent: sub-case 2a: add route exited $EXIT_CODE" >&2
    exit 1
fi

# Delete the route externally, simulating external or kernel removal.
ip route del 10.100.0.0/24 2>/dev/null || true

# Apply the policy without the route. The live route is already gone, so the diff
# sees no route to remove. Apply exits 0.
"$NETFYR_BIN" apply "$POLICY_NO_ROUTES"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-route-remove-nonexistent: sub-case 2b: absent-route removal exited $EXIT_CODE" >&2
    exit 1
fi

echo "PASS: 103-apply-route-remove-nonexistent"
