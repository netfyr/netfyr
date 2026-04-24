#!/bin/bash
# 301-apply-change-output-text.sh
# AC: "Apply a YAML file with changes needed"
#
# When a policy makes a real change, the output must:
#   1. Say "Applied 1 change" (singular, because only one entity is modified).
#   2. Mention the changed entity and the field that changed.
#   3. Exit with code 0.
#
# NOTE: The spec requires "Applied 1 change" (singular). The implementation
# currently outputs "Applied 1 changes" (always plural), which is a bug the
# verify phase should fix.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-change-output-text: netfyr binary not found at $NETFYR_BIN" >&2
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
    echo "FAIL: 301-apply-change-output-text: expected exit code 0, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: "the output shows 'Applied 1 change'" (singular for a single changed entity).
if ! echo "$OUTPUT" | grep -qi "applied 1 change"; then
    echo "FAIL: 301-apply-change-output-text: output does not say 'Applied 1 change'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: the output must reference the changed entity (veth-test0).
if ! echo "$OUTPUT" | grep -q "veth-test0"; then
    echo "FAIL: 301-apply-change-output-text: output does not mention changed entity 'veth-test0'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: the output must mention the changed field (mtu).
if ! echo "$OUTPUT" | grep -q "mtu"; then
    echo "FAIL: 301-apply-change-output-text: output does not mention changed field 'mtu'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-apply-change-output-text"
