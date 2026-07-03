#!/bin/bash
# 301-apply-with-address.sh
# Integration test: Apply a bare-state YAML policy setting MTU and an IP address.
# Mapped to spec shell scenario: "Apply with address in namespace".

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-with-address: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode: ensure no daemon socket is consulted.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
mtu: 1400
ipv4:
  addresses:
    - 10.99.0.1/24
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 301-apply-with-address: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 301-apply-with-address: veth-test0 does not have mtu 1400" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

assert_has_address veth-test0 "10.99.0.1/24"

echo "PASS: 301-apply-with-address"
