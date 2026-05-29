#!/bin/bash
# 355-diagnose-drift.sh -- End-to-end: netfyr diagnose detects configuration drift.
#
# Spec test 29: Verifies that `netfyr diagnose` detects when an external MTU
# change has drifted the system from policy.
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

# Apply a policy setting mtu=1400 on veth-e2e0.
cat > "$POLICY_DIR/policy.yaml" <<'EOF'
kind: policy
name: e2e-diagnose-drift
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
    echo "FAIL: 355-diagnose-drift: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Change mtu externally to 1500 (simulates drift).
ip link set veth-e2e0 mtu 1500

# Wait for debounce (daemon records external change).
sleep 1

# Run diagnose filtered to veth-e2e0.
DIAGNOSE_OUTPUT=""
DIAGNOSE_EXIT=0
DIAGNOSE_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" diagnose -s name=veth-e2e0 2>&1) || DIAGNOSE_EXIT=$?

# Verify exit code is 1 (warning).
if [[ $DIAGNOSE_EXIT -ne 1 ]]; then
    echo "FAIL: 355-diagnose-drift: expected exit code 1 (warning), got $DIAGNOSE_EXIT" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output contains "configuration drift".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "configuration.drift\|configuration drift"; then
    echo "FAIL: 355-diagnose-drift: output does not contain 'configuration drift'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output contains "warning".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "warning"; then
    echo "FAIL: 355-diagnose-drift: output does not mention 'warning'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output mentions mtu=1400 (policy) and mtu=1500 (system).
if ! echo "$DIAGNOSE_OUTPUT" | grep -q "1400"; then
    echo "FAIL: 355-diagnose-drift: output does not mention policy mtu 1400" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

if ! echo "$DIAGNOSE_OUTPUT" | grep -q "1500"; then
    echo "FAIL: 355-diagnose-drift: output does not mention current mtu 1500" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify output suggests `netfyr apply` to re-converge.
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "netfyr apply"; then
    echo "FAIL: 355-diagnose-drift: output does not suggest 'netfyr apply'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 355-diagnose-drift"
