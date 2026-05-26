#!/bin/bash
# 301-daemon-addr-twenty.sh -- Daemon mode: twenty addresses applied in order.
#
# Scenario 10: Stress test with many addresses. Creates veth pair, starts
# daemon, applies a policy with 20 addresses (10.99.0.1/24 through
# 10.99.0.20/24), verifies all 20 are present and in order.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-twenty.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-addr0 veth-addr1

start_daemon

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: addr-twenty
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
    - "10.99.0.1/24"
    - "10.99.0.2/24"
    - "10.99.0.3/24"
    - "10.99.0.4/24"
    - "10.99.0.5/24"
    - "10.99.0.6/24"
    - "10.99.0.7/24"
    - "10.99.0.8/24"
    - "10.99.0.9/24"
    - "10.99.0.10/24"
    - "10.99.0.11/24"
    - "10.99.0.12/24"
    - "10.99.0.13/24"
    - "10.99.0.14/24"
    - "10.99.0.15/24"
    - "10.99.0.16/24"
    - "10.99.0.17/24"
    - "10.99.0.18/24"
    - "10.99.0.19/24"
    - "10.99.0.20/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-twenty: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify all 20 addresses are present in kernel state.
for i in $(seq 1 20); do
    assert_has_address veth-addr0 "10.99.0.$i/24"
done
assert_address_count veth-addr0 20

# Verify ordering via netfyr query -o json.
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
assert_json_address_order "$QUERY_OUTPUT" \
    "10.99.0.1/24" "10.99.0.2/24" "10.99.0.3/24" "10.99.0.4/24" "10.99.0.5/24" \
    "10.99.0.6/24" "10.99.0.7/24" "10.99.0.8/24" "10.99.0.9/24" "10.99.0.10/24" \
    "10.99.0.11/24" "10.99.0.12/24" "10.99.0.13/24" "10.99.0.14/24" "10.99.0.15/24" \
    "10.99.0.16/24" "10.99.0.17/24" "10.99.0.18/24" "10.99.0.19/24" "10.99.0.20/24"

echo "PASS: 301-daemon-addr-twenty"
