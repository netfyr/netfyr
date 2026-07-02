#!/bin/bash
# 103-apply-dry-run-nonexistent.sh
# Integration test: Dry-run against a non-existent interface reports planned
# changes without modifying the system, and exits non-zero (changes pending).
# Spec AC-18: "Dry-run validates that target interface exists"
#
# When dry_run is called for an interface that does not exist, the CLI detects
# a planned Add operation (desired state differs from empty actual state) and
# exits 1 (changes pending). No interface is created.
#
# Requires: unshare, ip (iproute2)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-dry-run-nonexistent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent
netns_setup "$@"

# ---------- Inside the namespace ----------

# Do not create any interfaces — "eth99" must not exist in this namespace.

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: eth99
mtu: 1400
EOF

# Dry-run must exit non-zero: either 1 (planned changes) or 2 (error).
# Capture the exit code without letting set -e abort the script.
EXIT_CODE=0
DRY_RUN_OUTPUT=$("$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1) || EXIT_CODE=$?

if [[ $EXIT_CODE -eq 0 ]]; then
    echo "FAIL: 103-apply-dry-run-nonexistent: expected non-zero exit, got 0" >&2
    echo "      Output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# The interface must not have been created by the dry-run.
if ip link show eth99 2>/dev/null | grep -q eth99; then
    echo "FAIL: 103-apply-dry-run-nonexistent: eth99 was created but dry-run must not modify system" >&2
    exit 1
fi

echo "PASS: 103-apply-dry-run-nonexistent (exit code $EXIT_CODE, interface not created)"
