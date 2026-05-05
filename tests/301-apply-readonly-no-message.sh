#!/bin/bash
# 301-apply-readonly-no-message.sh
# AC: "Read-only fields must not produce any output message"
#
# When a policy changes a writable field on an interface whose actual state
# includes read-only fields (name, mac, carrier, etc.), the apply output
# must NOT contain "read-only field" messages.  The writable change must
# still be applied and exit code must be 0.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-readonly-no-message: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-test0
mtu: 1400
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) || EXIT_CODE=$?

rm -f "$POLICY_FILE"

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 301-apply-readonly-no-message: expected exit code 0, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must NOT contain "read-only" messages.
if echo "$OUTPUT" | grep -qi "read-only"; then
    echo "FAIL: 301-apply-readonly-no-message: output contains 'read-only' message" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: the writable change (mtu) must still be applied.
assert_mtu veth-test0 1400

echo "PASS: 301-apply-readonly-no-message"
