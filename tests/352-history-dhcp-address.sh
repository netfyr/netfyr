#!/bin/bash
# 352-history-dhcp-address.sh -- DHCP acquired address appears by value in CHANGES column.
#
# Spec test 54: netfyr history shows "+10.99.1.X/24" not just a count for DHCP entries.
#
# Requires: unshare, ip (iproute2), dnsmasq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 352-history-dhcp-address: 'dnsmasq' not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

# Create veth pair: veth-dhcp0 is client, veth-dhcp1 is server.
setup_dhcp_topology veth-dhcp0 veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

start_daemon

# Write and apply a DHCPv4 policy for veth-dhcp0.
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-dhcp-hist
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-dhcp-address: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease to be acquired (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# Give daemon time to finish reconciliation and journal writes.
sleep 2

# Run history and look for the dhcp-acquire line.
HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 10 2>&1)

DHCP_LINE=$(echo "$HISTORY_OUTPUT" | grep "dhcp-acquire" | head -n 1)

if [[ -z "$DHCP_LINE" ]]; then
    echo "FAIL: 352-history-dhcp-address: no dhcp-acquire entry found in history output" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# CHANGES column must show the acquired address by value (+10.99.1.X/24).
if ! echo "$DHCP_LINE" | grep -qP '\+10\.99\.1\.\d+/24'; then
    echo "FAIL: 352-history-dhcp-address: CHANGES does not show acquired address (+10.99.1.x/24)" >&2
    echo "      dhcp line: $DHCP_LINE" >&2
    echo "      full output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-dhcp-address"
