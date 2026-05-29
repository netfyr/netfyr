#!/bin/bash
# 355-diagnose-carrier.sh -- End-to-end: netfyr diagnose detects carrier loss.
#
# Spec test 31: Verifies that `netfyr diagnose` detects carrier loss when the
# peer end of a veth pair is brought down.
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

# Apply a policy on veth-e2e0 so the daemon manages it and records state.
cat > "$POLICY_DIR/policy.yaml" <<'EOF'
kind: policy
name: e2e-diagnose-carrier
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
    echo "FAIL: 355-diagnose-carrier: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Bring down the peer end to simulate carrier loss.
ip link set veth-e2e1 down

# Wait for debounce (daemon records external change with carrier=false).
sleep 1

# Run diagnose filtered to veth-e2e0.
DIAGNOSE_OUTPUT=""
DIAGNOSE_EXIT=0
DIAGNOSE_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" diagnose -s name=veth-e2e0 2>&1) || DIAGNOSE_EXIT=$?

# Verify exit code is 2 (critical).
if [[ $DIAGNOSE_EXIT -ne 2 ]]; then
    echo "FAIL: 355-diagnose-carrier: expected exit code 2 (critical), got $DIAGNOSE_EXIT" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output contains "carrier loss" or "carrier_loss".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "carrier.loss\|carrier loss"; then
    echo "FAIL: 355-diagnose-carrier: output does not contain 'carrier loss'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output contains "critical".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "critical"; then
    echo "FAIL: 355-diagnose-carrier: output does not mention 'critical'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 355-diagnose-carrier"
