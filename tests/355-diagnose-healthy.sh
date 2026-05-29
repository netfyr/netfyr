#!/bin/bash
# 355-diagnose-healthy.sh -- End-to-end: netfyr diagnose reports healthy when no drift.
#
# Spec test 30: Verifies that `netfyr diagnose` reports healthy when the system
# state matches the applied policy.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# Apply a policy setting mtu=1400 on veth-e2e0 (no external changes).
cat > "$POLICY_DIR/policy.yaml" <<'EOF'
kind: policy
name: e2e-diagnose-healthy
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_DIR/policy.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 355-diagnose-healthy: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run diagnose filtered to veth-e2e0.
DIAGNOSE_OUTPUT=""
DIAGNOSE_EXIT=0
DIAGNOSE_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" diagnose -s name=veth-e2e0 2>&1) || DIAGNOSE_EXIT=$?

# Verify exit code is 0 (healthy).
if [[ $DIAGNOSE_EXIT -ne 0 ]]; then
    echo "FAIL: 355-diagnose-healthy: expected exit code 0 (healthy), got $DIAGNOSE_EXIT" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output contains "healthy".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "healthy"; then
    echo "FAIL: 355-diagnose-healthy: output does not contain 'healthy'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 355-diagnose-healthy"
