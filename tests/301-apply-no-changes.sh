#!/bin/bash
# 301-apply-no-changes.sh
# AC: "Apply a single YAML policy file" — when the policy already matches the
# current system state, the output says "No changes needed" and exit code is 0.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-no-changes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
# mtu=1500 matches the default veth MTU — no change should be needed.
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
mtu: 1500
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) || EXIT_CODE=$?

rm -f "$POLICY_FILE"

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 301-apply-no-changes: expected exit code 0 (no changes), got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

if ! echo "$OUTPUT" | grep -qi "no changes needed"; then
    echo "FAIL: 301-apply-no-changes: output does not say 'No changes needed'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-apply-no-changes"
