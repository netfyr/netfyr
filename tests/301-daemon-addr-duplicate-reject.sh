#!/bin/bash
# 301-daemon-addr-duplicate-reject.sh -- Daemon mode: duplicate addresses in
# YAML are rejected with a validation error.
#
# Scenario 13: Creates veth pair, starts daemon. Writes a policy with
# duplicate addresses (10.99.0.1/24 appears twice). Runs netfyr apply.
# Verifies non-zero exit code, error mentions "duplicate", and no addresses
# were applied to the interface.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-duplicate-reject.sh
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
name: addr-dup-reject
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  ipv4:
    addresses:
      - "10.99.0.1/24"
      - "10.99.0.2/24"
      - "10.99.0.1/24"
EOF

# The spec expects exit code 2 for a validation error; check for any non-zero.
APPLY_EXIT=0
APPLY_OUTPUT=$("$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) || APPLY_EXIT=$?

if [[ $APPLY_EXIT -eq 0 ]]; then
    echo "FAIL: 301-daemon-addr-duplicate-reject: netfyr apply should have failed but exited 0" >&2
    echo "      output: $APPLY_OUTPUT" >&2
    exit 1
fi

# Error output must mention the duplicate.
if ! echo "$APPLY_OUTPUT" | grep -qi "duplicate"; then
    echo "FAIL: 301-daemon-addr-duplicate-reject: error output does not mention 'duplicate'" >&2
    echo "      output: $APPLY_OUTPUT" >&2
    exit 1
fi

# Nothing should have been applied: the interface must have no inet addresses.
assert_address_count veth-addr0 0

echo "PASS: 301-daemon-addr-duplicate-reject"
