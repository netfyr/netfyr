#!/bin/bash
# 600-e2e-dhcp-and-static.sh -- End-to-end: DHCP and static policies on separate interfaces coexist.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-dhcp-and-static.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-and-static: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

require_binaries
netns_setup "$@"
daemon_test_setup
setup_journal

# Static interface pair.
create_veth veth-static0 veth-static1

# DHCP interface pair: veth-dhcp1 is the server side.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24

# Start dnsmasq DHCP server on the server-side interface.
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

start_daemon

# Write two policy files: static for veth-static0 and DHCPv4 for veth-dhcp0.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/static.yaml" <<'EOF'
kind: policy
name: e2e-ds-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-static0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-ds-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply both policies atomically from the directory.
APPLY_EXIT=0
"$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-and-static: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait up to 10 seconds for the DHCP lease to appear on veth-dhcp0.
wait_for_address veth-dhcp0 "10.99.1." 10

# Verify the static interface is correctly configured.
assert_mtu veth-static0 1400
assert_has_address veth-static0 "10.99.0.1"

# Verify the DHCP interface acquired an address in the expected range.
assert_has_address veth-dhcp0 "10.99.1."

# Cross-contamination checks: static address must not be on the DHCP interface and vice versa.
assert_not_has_address veth-dhcp0 "10.99.0."
assert_not_has_address veth-static0 "10.99.1."

# Give daemon time to finish reconciliation and journal writes.
sleep 2

# Verify the DHCP lease event was recorded in the journal with trigger "dhcp-acquire".
HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 5 2>&1)
if ! echo "$HISTORY_OUTPUT" | grep -qF "dhcp-acquire"; then
    echo "FAIL: 600-e2e-dhcp-and-static: history does not contain dhcp-acquire trigger" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-dhcp-and-static"
