#!/bin/bash
# 103-apply-read-only-field.sh
# Integration test: Modify operation skips read-only fields (defensive check).
# Spec scenario: "Modify operation skips read-only fields (defensive)"
#
# The field "carrier" is read-only (x-netfyr-writable: false in the schema).
# When a policy sets carrier=false on a veth interface that has carrier=true,
# the backend must skip the carrier field rather than fail. The apply must
# succeed (exit 0) and the carrier must remain unchanged (still true).
#
# Note: Both ends of the veth pair are brought up by create_veth, so the
# carrier is true. Requesting carrier=false triggers the defensive read-only
# check in apply_modify_fields (Phase 4), which emits a SkippedOperation.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-read-only-field: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# carrier is a read-only field. Setting it to false in the policy should
# cause the backend to skip it (not fail), leaving the carrier unchanged.
POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
carrier: false
EOF

"$NETFYR_BIN" apply "$POLICY_FILE" 2>&1
EXIT_CODE=$?

# The apply must succeed (read-only field skip is not a failure).
if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-read-only-field: expected exit code 0, got $EXIT_CODE" >&2
    echo "      (read-only field skip must not be reported as a failure)" >&2
    exit 1
fi

# The carrier must still be true — the read-only field was not modified.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -qE "(state UP|<[^>]*UP[^>]*>)"; then
    echo "FAIL: 103-apply-read-only-field: interface veth-test0 is not UP after apply" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-read-only-field"
