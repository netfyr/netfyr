#!/bin/bash
# 301-daemon-addr-removal.sh -- Daemon mode: address removal via replace-all.
#
# Scenario 15: Creates veth pair, starts daemon. First apply: policy with
# mtu=1400 and 3 addresses. Verifies addresses present. Second apply: new
# policy with only mtu=1400 (no addresses field). Verifies all 3 addresses
# are removed and mtu is still 1400.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-removal.sh
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

# ── Phase 1: Apply policy with mtu=1400 and 3 addresses ──────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: addr-removal
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  mtu: 1400
  ipv4:
    addresses:
      - "10.99.0.1/24"
      - "10.99.0.2/24"
      - "10.99.0.3/24"
EOF

APPLY_A_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_A_EXIT=$?
if [[ $APPLY_A_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-removal: first apply exited with code $APPLY_A_EXIT" >&2
    exit 1
fi

assert_mtu veth-addr0 1400
assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_address_count veth-addr0 3

# ── Phase 2: Apply policy with only mtu=1400 (no addresses) ──────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: addr-removal
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  mtu: 1400
  ipv4: {}
EOF

APPLY_B_EXIT=0
"$NETFYR_BIN" apply "$POLICY_B" || APPLY_B_EXIT=$?
if [[ $APPLY_B_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-removal: second apply exited with code $APPLY_B_EXIT" >&2
    exit 1
fi

# Verify all 3 addresses are removed.
assert_not_has_address veth-addr0 "10.99.0.1"
assert_not_has_address veth-addr0 "10.99.0.2"
assert_not_has_address veth-addr0 "10.99.0.3"
assert_address_count veth-addr0 0

# Verify mtu is still 1400 (only addresses were removed).
assert_mtu veth-addr0 1400

echo "PASS: 301-daemon-addr-removal"
