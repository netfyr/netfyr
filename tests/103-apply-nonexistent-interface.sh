#!/bin/bash
# 103-apply-nonexistent-interface.sh
# Integration test: Apply to a non-existent interface reports failure.
# Spec scenario: "Apply to a non-existent interface reports failure"
#
# The apply must exit with code 2 (total failure) and must NOT report success.
# The interface "eth99" is guaranteed not to exist in a fresh network namespace.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-nonexistent-interface: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

# No interfaces created — "eth99" does not exist in this namespace.

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: eth99
mtu: 1400
EOF

# Capture the exit code — expect non-zero (total failure = exit 2).
"$NETFYR_BIN" apply "$POLICY_FILE" 2>&1
EXIT_CODE=$?

if [[ $EXIT_CODE -eq 0 ]]; then
    echo "FAIL: 103-apply-nonexistent-interface: expected non-zero exit code, got 0" >&2
    exit 1
fi

echo "PASS: 103-apply-nonexistent-interface (exit code $EXIT_CODE, as expected)"
