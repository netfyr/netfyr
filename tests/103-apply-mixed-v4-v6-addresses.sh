#!/bin/bash
# 103-apply-mixed-v4-v6-addresses.sh
# Integration test: Apply a policy with both IPv4 and IPv6 addresses on
# the same interface and verify both are present.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-mixed-v4-v6-addresses: netfyr binary not found at $NETFYR_BIN" >&2
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
addresses:
  - "10.99.0.1/24"
  - "fd00:aa::1/64"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-mixed-v4-v6-addresses: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

assert_has_address veth-test0 "10.99.0.1/24"

ADDR6_OUTPUT=$(ip -6 addr show dev veth-test0)
if ! echo "$ADDR6_OUTPUT" | grep -q "fd00:aa::1/64"; then
    echo "FAIL: 103-apply-mixed-v4-v6-addresses: IPv6 address fd00:aa::1/64 not found" >&2
    echo "      ip -6 addr output: $ADDR6_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-mixed-v4-v6-addresses"
