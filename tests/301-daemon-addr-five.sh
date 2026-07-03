#!/bin/bash
# 301-daemon-addr-five.sh -- Daemon mode: five addresses applied in order.
#
# Scenario 9: Creates veth pair, starts daemon, applies a policy with 5
# addresses (10.99.0.1/24 through 10.99.0.5/24), verifies all 5 are present
# and in order via ip addr show and netfyr query -o json.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-five.sh
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
name: addr-five
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  ipv4:
    addresses:
      - "10.99.0.1/24"
      - "10.99.0.2/24"
      - "10.99.0.3/24"
      - "10.99.0.4/24"
      - "10.99.0.5/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-five: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify kernel state: all 5 addresses present.
assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_has_address veth-addr0 "10.99.0.4/24"
assert_has_address veth-addr0 "10.99.0.5/24"
assert_address_count veth-addr0 5

# Verify ordering via netfyr query -o json.
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
assert_json_address_order "$QUERY_OUTPUT" \
    "10.99.0.1/24" "10.99.0.2/24" "10.99.0.3/24" "10.99.0.4/24" "10.99.0.5/24"

echo "PASS: 301-daemon-addr-five"
