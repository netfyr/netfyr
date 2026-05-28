#!/bin/bash
# 103-apply-add-remove-address.sh
# Integration test: Add then remove an IP address via netfyr apply.
# Mapped to spec shell scenario: "Add and remove IP addresses in namespace".

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-add-remove-address: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Phase 1: Apply policy that adds the address.
POLICY_ADD=$(mktemp --suffix=.yaml)
cat > "$POLICY_ADD" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.0.1/24"
EOF

"$NETFYR_BIN" apply "$POLICY_ADD"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-add-remove-address: apply (add) exited with code $EXIT_CODE" >&2
    exit 1
fi

ADDR_OUTPUT=$(ip addr show veth-test0)
if ! echo "$ADDR_OUTPUT" | grep -q "10.99.0.1/24"; then
    echo "FAIL: 103-apply-add-remove-address: address 10.99.0.1/24 not found after add" >&2
    echo "      ip addr output: $ADDR_OUTPUT" >&2
    exit 1
fi

# Phase 2: Apply policy without addresses field — triggers removal via removed_fields.
POLICY_REMOVE=$(mktemp --suffix=.yaml)
cat > "$POLICY_REMOVE" <<'EOF'
selector:
  name: veth-test0
EOF

"$NETFYR_BIN" apply "$POLICY_REMOVE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-add-remove-address: apply (remove) exited with code $EXIT_CODE" >&2
    exit 1
fi

ADDR_OUTPUT=$(ip addr show veth-test0)
if echo "$ADDR_OUTPUT" | grep -q "10.99.0.1/24"; then
    echo "FAIL: 103-apply-add-remove-address: address 10.99.0.1/24 still present after remove" >&2
    echo "      ip addr output: $ADDR_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-add-remove-address"
