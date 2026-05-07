#!/bin/bash
# 103-apply-ipv6-link-local-preserved.sh
# Integration test: Verify that applying a policy with an IPv6 address
# does not remove a pre-existing link-local address (fe80::/10).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-ipv6-link-local-preserved: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# Manually add a link-local address.
add_address veth-test0 fe80::1/64

# Verify it's there before apply.
ADDR_BEFORE=$(ip -6 addr show dev veth-test0)
if ! echo "$ADDR_BEFORE" | grep -q "fe80::1/64"; then
    echo "FAIL: 103-apply-ipv6-link-local-preserved: fe80::1/64 not present before apply" >&2
    exit 1
fi

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
addresses:
  - "fd00:aa::1/64"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-ipv6-link-local-preserved: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

# The link-local address must still be present after apply.
ADDR_AFTER=$(ip -6 addr show dev veth-test0)
if ! echo "$ADDR_AFTER" | grep -q "fe80::1/64"; then
    echo "FAIL: 103-apply-ipv6-link-local-preserved: fe80::1/64 was removed by apply" >&2
    echo "      ip -6 addr output: $ADDR_AFTER" >&2
    exit 1
fi

# The global address must also be present.
if ! echo "$ADDR_AFTER" | grep -q "fd00:aa::1/64"; then
    echo "FAIL: 103-apply-ipv6-link-local-preserved: fd00:aa::1/64 not found after apply" >&2
    echo "      ip -6 addr output: $ADDR_AFTER" >&2
    exit 1
fi

echo "PASS: 103-apply-ipv6-link-local-preserved"
