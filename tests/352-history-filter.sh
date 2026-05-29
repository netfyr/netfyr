#!/bin/bash
# 352-history-filter.sh -- History -s name=X filters entries by entity name.
#
# Spec test 21: netfyr history -s name=veth-a0 shows only entries for veth-a0.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-a0 veth-a1
create_veth veth-b0 veth-b1
start_daemon

# Apply policy for veth-a0.
POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-filter-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-a0
  mtu: 1400
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-filter: apply for veth-a0 exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Apply policy for veth-b0 (separate apply = separate journal entry).
POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-filter-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-b0
  mtu: 1300
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-filter: apply for veth-b0 exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run history filtered to veth-a0.
FILTER_OUTPUT=$("$NETFYR_BIN" history -s name=veth-a0 -n 20 2>&1)

if ! echo "$FILTER_OUTPUT" | grep -q "veth-a0"; then
    echo "FAIL: 352-history-filter: filtered output does not contain 'veth-a0'" >&2
    echo "      output: $FILTER_OUTPUT" >&2
    exit 1
fi

if echo "$FILTER_OUTPUT" | grep -q "veth-b0"; then
    echo "FAIL: 352-history-filter: filtered output unexpectedly contains 'veth-b0'" >&2
    echo "      output: $FILTER_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-filter"
