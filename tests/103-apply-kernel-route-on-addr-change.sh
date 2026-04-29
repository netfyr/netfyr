#!/bin/bash
# 103-apply-kernel-route-on-addr-change.sh
# Integration test: When a static policy changes the interface address, the
# old kernel prefix route must disappear and the new one must appear.
# Mapped to spec: "Prefix routes track address changes".
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/103-apply-kernel-route-on-addr-change.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Set up initial address and route.
add_address veth-test0 10.99.70.1/24
ip route add default via 10.99.70.254 dev veth-test0

# Precondition: old prefix route must exist.
ROUTES_BEFORE=$(ip route)
if ! echo "$ROUTES_BEFORE" | grep -q "10.99.70.0/24"; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: precondition: old prefix route not found" >&2
    echo "      ip route: $ROUTES_BEFORE" >&2
    exit 1
fi

# Apply a policy with a new address and new default gateway.
POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
addresses:
  - "10.99.71.1/24"
routes:
  - destination: "0.0.0.0/0"
    gateway: "10.99.71.254"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

ROUTES_AFTER=$(ip route)

# Old prefix route must be gone (kernel removed it with the old address).
if echo "$ROUTES_AFTER" | grep -q "10.99.70.0/24"; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: old prefix route 10.99.70.0/24 still present" >&2
    echo "      ip route: $ROUTES_AFTER" >&2
    exit 1
fi

# New prefix route must exist (kernel added it for the new address).
if ! echo "$ROUTES_AFTER" | grep -q "10.99.71.0/24"; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: new prefix route 10.99.71.0/24 not found" >&2
    echo "      ip route: $ROUTES_AFTER" >&2
    exit 1
fi

# New default route must exist.
if ! echo "$ROUTES_AFTER" | grep -q "default via 10.99.71.254"; then
    echo "FAIL: 103-apply-kernel-route-on-addr-change: new default route via 10.99.71.254 not found" >&2
    echo "      ip route: $ROUTES_AFTER" >&2
    exit 1
fi

echo "PASS: 103-apply-kernel-route-on-addr-change"
