#!/bin/bash
# 301-daemon-dry-run.sh -- Daemon mode: dry-run shows changes without applying.
#
# Scenario 6: Creates veth pair, starts daemon, runs netfyr apply --dry-run
# with a policy setting mtu=1400. Verifies the interface still has the
# default mtu (not 1400) and the dry-run output mentions the mtu change.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-dry-run.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-e2e0 veth-e2e1

start_daemon

# Write a policy that would set mtu=1400.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dry-run-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$("$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1) || DRY_RUN_EXIT=$?

# Dry-run with changes pending exits 1; with no changes exits 0.
# Either way, the kernel state must not be mutated.
# (We expect exit 1 here since mtu change is pending, but 0 is also acceptable
#  if the implementation reports no-changes for a fresh interface.)

# Verify the interface still has the DEFAULT mtu (not 1400).
LINK_OUTPUT=$(ip link show veth-e2e0 2>&1) || true
if echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 301-daemon-dry-run: mtu was changed to 1400 by dry-run (should not mutate state)" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

# Verify the dry-run output mentions "mtu" (the pending change).
if ! echo "$DRY_RUN_OUTPUT" | grep -qi "mtu"; then
    echo "FAIL: 301-daemon-dry-run: dry-run output does not mention 'mtu'" >&2
    echo "      dry-run output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-daemon-dry-run"
