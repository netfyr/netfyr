#!/bin/bash
# 103-apply-addr-idempotent-remove.sh
# Integration test: Removing addresses is idempotent — when the desired address
# list is already satisfied (nothing to remove), apply exits 0 without failure.
# Spec AC-11: "Removing a non-existent address is idempotent"
#
# The specific code path (find_address_message returns None → SkippedOperation
# "not present") is a race-condition scenario that unit tests cover directly.
# This shell test verifies the end-to-end exit-code guarantee across two
# sub-cases:
#   1. Apply "no addresses" policy to an interface that already has no addresses.
#   2. Apply "no addresses" policy, then apply it again (idempotent second pass).
#
# Requires: unshare, ip (iproute2)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-addr-idempotent-remove: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent
netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_EMPTY=$(mktemp --suffix=.yaml)
cat > "$POLICY_EMPTY" <<'EOF'
selector:
  name: veth-test0
addresses: []
EOF

# --- Sub-case 1: "no addresses" policy applied to interface with no addresses ---
#
# Interface starts bare (create_veth does not assign any addresses).
# Desired: [], current: [] → nothing to remove → exit 0.
"$NETFYR_BIN" apply "$POLICY_EMPTY"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent-remove: sub-case 1: empty apply on bare interface exited $EXIT_CODE" >&2
    exit 1
fi

# --- Sub-case 2: remove an address, then apply the same "no addresses" policy again ---
#
# First pass: interface has 10.99.77.1/24; apply removes it (normal removal).
# Second pass: interface has no addresses; apply finds nothing to remove → exit 0.
# This verifies that a "remove" policy is idempotent: running it twice does not
# fail on the second invocation.
add_address veth-test0 10.99.77.1/24

"$NETFYR_BIN" apply "$POLICY_EMPTY"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent-remove: sub-case 2a: first removal exited $EXIT_CODE" >&2
    exit 1
fi
assert_not_has_address veth-test0 "10.99.77.1/24"

# Second application — interface has no addresses, nothing to remove.
"$NETFYR_BIN" apply "$POLICY_EMPTY"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent-remove: sub-case 2b: idempotent removal exited $EXIT_CODE" >&2
    exit 1
fi
assert_not_has_address veth-test0 "10.99.77.1/24"

echo "PASS: 103-apply-addr-idempotent-remove"
