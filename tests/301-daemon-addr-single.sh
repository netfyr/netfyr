#!/bin/bash
# 301-daemon-addr-single.sh -- Daemon mode: single address applied and verified.
#
# Scenario 8: Creates veth pair, starts daemon, applies a policy with one
# address 10.99.0.1/24, verifies the address is present and the count is 1,
# then verifies address order via netfyr query -o json.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-single.sh
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
name: addr-single
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-single: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify kernel state.
assert_has_address veth-addr0 "10.99.0.1/24"
assert_address_count veth-addr0 1

# Verify address order via netfyr query -o json.
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
assert_json_address_order "$QUERY_OUTPUT" "10.99.0.1/24"

echo "PASS: 301-daemon-addr-single"
