#!/bin/bash
# 103-apply-addr-idempotent.sh
# Integration test: Applying an address policy to an interface that already has
# the same address is idempotent — the command exits 0 and the address is retained.
# Spec scenario: "Adding an already-existing address is idempotent" (AC-7).
#
# Two sub-cases are verified:
#   1. Interface address added externally (ip addr add), then policy applied.
#   2. Same policy applied twice in sequence; second invocation exits 0.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-addr-idempotent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
addresses:
  - "10.99.0.1/24"
EOF

# --- Sub-case 1: address already present via ip addr add ---
#
# Add the address directly so the interface already has it before apply runs.
# The first apply must: treat the existing address as already satisfied and exit 0.
add_address veth-test0 10.99.0.1/24

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent: sub-case 1: expected exit 0 when address already present, got $EXIT_CODE" >&2
    exit 1
fi
assert_has_address veth-test0 "10.99.0.1/24"

# --- Sub-case 2: same policy applied twice ---
#
# Remove the address so the first apply starts fresh, then apply twice.
# The second apply must exit 0 even though there is nothing to change.
ip addr del 10.99.0.1/24 dev veth-test0

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent: sub-case 2a: first apply failed with $EXIT_CODE" >&2
    exit 1
fi

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-addr-idempotent: sub-case 2b: second (idempotent) apply failed with $EXIT_CODE" >&2
    exit 1
fi
assert_has_address veth-test0 "10.99.0.1/24"

echo "PASS: 103-apply-addr-idempotent"
