#!/bin/bash
# 352-history-show-route-gateway.sh -- History --show displays route with gateway in diff.
#
# Spec test 57: netfyr history --show shows route destination and gateway.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 352-history-show-route-gateway: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
add_address veth-e2e0 10.99.0.1/24
start_daemon

# Apply a static policy with a route that has a gateway.
# May fail at the kernel level (gateway unreachable) but the journal is still written.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-gw-route
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "10.200.0.0/16"
      gateway: "10.99.0.254"
      metric: 100
EOF

"$NETFYR_BIN" apply "$POLICY_FILE" 2>&1 || true

# Find the policy_apply entry seq.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: 352-history-show-route-gateway: could not find policy_apply entry in journal" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

SHOW_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# The diff section must contain the route destination.
if ! echo "$SHOW_OUTPUT" | grep -qF "10.200.0.0"; then
    echo "FAIL: 352-history-show-route-gateway: diff does not contain route destination '10.200.0.0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# The diff section must contain the gateway.
if ! echo "$SHOW_OUTPUT" | grep -qF "via 10.99.0.254"; then
    echo "FAIL: 352-history-show-route-gateway: diff does not contain gateway 'via 10.99.0.254'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-show-route-gateway"
