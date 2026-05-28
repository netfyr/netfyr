#!/bin/bash
# 103-apply-dry-run.sh
# Integration test: Dry-run reports planned changes without modifying the system.
# Spec scenario: "Dry-run reports planned changes without modifying the system"
#
# Sequence:
#   1. Create a veth interface with default mtu 1500.
#   2. Run `netfyr apply --dry-run` with a policy setting mtu=1400.
#   3. Verify the output mentions the planned change (mtu).
#   4. Verify the exit code is non-zero (changes are planned → exit 1).
#   5. Verify the system mtu is still 1500 (nothing was modified).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-dry-run: netfyr binary not found at $NETFYR_BIN" >&2
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
mtu: 1400
EOF

# Dry-run should exit non-zero (changes pending) and show the planned mtu change.
DRY_RUN_OUTPUT=$("$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1)
EXIT_CODE=$?

# The CLI exits 1 when there are pending changes, 0 when nothing would change.
if [[ $EXIT_CODE -eq 0 ]]; then
    echo "FAIL: 103-apply-dry-run: expected non-zero exit code (changes pending), got 0" >&2
    echo "      Output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# Output must mention the planned change — either "mtu" or "dry run".
if ! echo "$DRY_RUN_OUTPUT" | grep -iq "mtu\|dry run\|change"; then
    echo "FAIL: 103-apply-dry-run: output does not mention planned change" >&2
    echo "      Output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# The actual mtu must be unchanged (still 1500 — default for veth in a namespace).
assert_mtu veth-test0 1500

echo "PASS: 103-apply-dry-run"
