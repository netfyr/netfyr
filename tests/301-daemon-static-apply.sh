#!/bin/bash
# 301-daemon-static-apply.sh -- Daemon mode: static policy apply verified
# with ip commands and netfyr query.
#
# Scenario 1: Creates veth pair, starts daemon, applies a static policy
# setting mtu=1400 and address 10.99.0.1/24, then verifies kernel state
# and query output.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-static-apply.sh
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

# Write a static policy: mtu=1400 and address 10.99.0.1/24 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-static-apply: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify kernel state with ip commands.
assert_mtu veth-e2e0 1400
assert_has_address veth-e2e0 "10.99.0.1"

# Verify via netfyr query -o json (daemon-backed).
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-e2e0 -o json 2>&1)
if ! echo "$QUERY_OUTPUT" | grep -q '"mtu".*1400'; then
    echo "FAIL: 301-daemon-static-apply: netfyr query output does not show mtu=1400" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi
if ! echo "$QUERY_OUTPUT" | grep -q "10.99.0.1"; then
    echo "FAIL: 301-daemon-static-apply: netfyr query output does not show address 10.99.0.1" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-daemon-static-apply"
