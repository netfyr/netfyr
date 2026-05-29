#!/bin/bash
# 352-history-route-in-changes.sh -- Route changes appear in the CHANGES column.
#
# Spec test 56: netfyr history shows route information (count-only for non-default routes).
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
add_address veth-e2e0 10.99.0.1/24
start_daemon

# Apply a static policy with a non-default route.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-routes
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "10.100.0.0/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-route-in-changes: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 5 2>&1)

# The CHANGES column must contain the word "route".
if ! echo "$HISTORY_OUTPUT" | grep "apply (" | grep -qi "route"; then
    echo "FAIL: 352-history-route-in-changes: CHANGES column does not contain 'route'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-route-in-changes"
