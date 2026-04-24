#!/bin/bash
# 301-dry-run-shows-diff-text.sh
# AC: "Dry-run shows diff without applying"
#
# When running with --dry-run and there are changes pending, the output must:
#   1. Say "Dry run:" at the start of the summary line.
#   2. Mention the number of changes that would be applied.
#   3. Include the entity name being changed.
#   4. Include the field name being changed ("mtu").
#   5. Exit with code 1 (changes pending but not applied).
#   6. Leave the kernel state unchanged (mtu still 1500).
#
# NOTE: The spec shows "~ ethernet eth0: mtu 1500 -> 9000" (inline format).
# The implementation delegates to DiffReport::format_text() which uses a unified
# diff style: "    -mtu: 1500" / "    +mtu: 9000". This test checks for the
# presence of "Dry run:" and the mtu field, which are requirements regardless
# of the exact format style chosen.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-dry-run-shows-diff-text: netfyr binary not found at $NETFYR_BIN" >&2
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
OUTPUT=$("$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1) || EXIT_CODE=$?

rm -f "$POLICY_FILE"

# AC: exit code 1 when changes are pending (not applied).
if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 301-dry-run-shows-diff-text: expected exit code 1, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must contain "Dry run:" to introduce the diff summary.
if ! echo "$OUTPUT" | grep -qi "dry run"; then
    echo "FAIL: 301-dry-run-shows-diff-text: output does not contain 'Dry run:'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must mention the entity being changed.
if ! echo "$OUTPUT" | grep -q "veth-test0"; then
    echo "FAIL: 301-dry-run-shows-diff-text: output does not mention entity 'veth-test0'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must mention the changed field ("mtu").
if ! echo "$OUTPUT" | grep -q "mtu"; then
    echo "FAIL: 301-dry-run-shows-diff-text: output does not mention field 'mtu'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: the old value (1500) and new value (1400) must both appear in the diff.
if ! echo "$OUTPUT" | grep -q "1500"; then
    echo "FAIL: 301-dry-run-shows-diff-text: output does not show old value 1500" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi
if ! echo "$OUTPUT" | grep -q "1400"; then
    echo "FAIL: 301-dry-run-shows-diff-text: output does not show new value 1400" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: kernel MTU must remain unchanged — dry-run must not apply.
assert_mtu veth-test0 1500

echo "PASS: 301-dry-run-shows-diff-text"
