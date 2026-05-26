#!/bin/bash
# 301-daemon-addr-replace.sh -- Daemon mode: address replacement removes old
# addresses and adds new ones in order.
#
# Scenario 11: Creates veth pair, starts daemon. First apply: policy A with
# 5 addresses in 10.99.0.0/24. Second apply: policy B with 5 different
# addresses in 10.99.1.0/24. Verifies old addresses are gone and new ones
# are present in order.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-addr-replace.sh
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

# ── Phase 1: Apply policy A (5 addresses in 10.99.0.0/24) ────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: addr-replace
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
EOF

APPLY_A_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_A_EXIT=$?
if [[ $APPLY_A_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-replace: first apply exited with code $APPLY_A_EXIT" >&2
    exit 1
fi

# Verify all 5 old addresses are present.
assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_has_address veth-addr0 "10.99.0.4/24"
assert_has_address veth-addr0 "10.99.0.5/24"
assert_address_count veth-addr0 5

# ── Phase 2: Apply policy B (5 different addresses in 10.99.1.0/24) ──────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: addr-replace
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
    - "10.99.1.1/24"
    - "10.99.1.2/24"
    - "10.99.1.3/24"
    - "10.99.1.4/24"
    - "10.99.1.5/24"
EOF

APPLY_B_EXIT=0
"$NETFYR_BIN" apply "$POLICY_B" || APPLY_B_EXIT=$?
if [[ $APPLY_B_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-addr-replace: second apply exited with code $APPLY_B_EXIT" >&2
    exit 1
fi

# Verify old addresses are gone.
assert_not_has_address veth-addr0 "10.99.0.1"
assert_not_has_address veth-addr0 "10.99.0.2"
assert_not_has_address veth-addr0 "10.99.0.3"
assert_not_has_address veth-addr0 "10.99.0.4"
assert_not_has_address veth-addr0 "10.99.0.5"

# Verify new addresses are present and in order.
assert_has_address veth-addr0 "10.99.1.1/24"
assert_has_address veth-addr0 "10.99.1.2/24"
assert_has_address veth-addr0 "10.99.1.3/24"
assert_has_address veth-addr0 "10.99.1.4/24"
assert_has_address veth-addr0 "10.99.1.5/24"
assert_address_count veth-addr0 5

QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
assert_json_address_order "$QUERY_OUTPUT" \
    "10.99.1.1/24" "10.99.1.2/24" "10.99.1.3/24" "10.99.1.4/24" "10.99.1.5/24"

echo "PASS: 301-daemon-addr-replace"
