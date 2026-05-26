#!/bin/bash
# 103-apply-partial-failure.sh
# Integration test: Multiple operations where one succeeds and one fails.
# Spec scenario: "Multiple operations with partial failure"
#
# Two policies are applied simultaneously: one for a real veth interface
# (succeeds) and one for a non-existent "eth99" (fails).
# The apply must exit with code 1 (partial failure) and the real interface
# must be modified correctly.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-partial-failure: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

# Create one real veth pair; "eth99" does not exist.
create_veth veth-test0 veth-test1

TMPDIR_POLICY=$(mktemp -d)

# Policy 1: valid interface — should succeed.
cat > "$TMPDIR_POLICY/veth-test0.yaml" <<'EOF'
type: ethernet
name: veth-test0
mtu: 1400
EOF

# Policy 2: non-existent interface — should fail.
cat > "$TMPDIR_POLICY/eth99.yaml" <<'EOF'
type: ethernet
name: eth99
mtu: 9000
EOF

# Apply both policies at once via the directory.
"$NETFYR_BIN" apply "$TMPDIR_POLICY" 2>&1
EXIT_CODE=$?

# Exit code 1 signals partial failure (some succeeded, some failed).
if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 103-apply-partial-failure: expected exit code 1 (partial failure), got $EXIT_CODE" >&2
    exit 1
fi

# The real interface must have been modified (mtu=1400 applied successfully).
assert_mtu veth-test0 1400

echo "PASS: 103-apply-partial-failure"
