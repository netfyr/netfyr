#!/bin/bash
# 103-apply-preserves-kernel-route.sh
# Integration test: Applying a static policy with routes must not delete
# the kernel-generated prefix route for the interface's address.
# Mapped to spec: "Apply must not remove proto-kernel routes".
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/103-apply-preserves-kernel-route.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-preserves-kernel-route: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Assign an address so the kernel adds the connected prefix route.
add_address veth-test0 10.99.50.1/24

# Precondition: kernel prefix route must exist.
ROUTES_BEFORE=$(ip route)
if ! echo "$ROUTES_BEFORE" | grep -q "10.99.50.0/24"; then
    echo "FAIL: 103-apply-preserves-kernel-route: precondition: kernel prefix route 10.99.50.0/24 not found" >&2
    echo "      ip route: $ROUTES_BEFORE" >&2
    exit 1
fi

# Apply a static policy that sets addresses and a default route.
POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.50.1/24"
routes:
  - destination: "0.0.0.0/0"
    gateway: "10.99.50.254"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-preserves-kernel-route: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

# Verify the kernel prefix route survived.
ROUTES_AFTER=$(ip route)

if ! echo "$ROUTES_AFTER" | grep -q "10.99.50.0/24"; then
    echo "FAIL: 103-apply-preserves-kernel-route: kernel prefix route 10.99.50.0/24 was deleted" >&2
    echo "      ip route: $ROUTES_AFTER" >&2
    exit 1
fi

# Verify the desired default route was added.
if ! echo "$ROUTES_AFTER" | grep -q "default via 10.99.50.254"; then
    echo "FAIL: 103-apply-preserves-kernel-route: default route via 10.99.50.254 not found" >&2
    echo "      ip route: $ROUTES_AFTER" >&2
    exit 1
fi

echo "PASS: 103-apply-preserves-kernel-route"
