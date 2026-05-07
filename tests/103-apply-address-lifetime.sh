#!/bin/bash
# 103-apply-address-lifetime.sh
# Integration test: Apply a policy with an IPv4 address that has
# valid_lft and preferred_lft, then verify the kernel shows a finite
# lifetime (not "forever").

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-address-lifetime: netfyr binary not found at $NETFYR_BIN" >&2
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
  - address: "10.99.0.1/24"
    valid_lft: 120
    preferred_lft: 60
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-address-lifetime: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

assert_has_address veth-test0 "10.99.0.1/24"
assert_valid_lft_finite veth-test0 "10.99.0.1"

echo "PASS: 103-apply-address-lifetime"
