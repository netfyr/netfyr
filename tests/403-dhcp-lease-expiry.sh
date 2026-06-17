#!/bin/bash
# 403-dhcp-lease-expiry.sh
# Integration test: When a DHCP lease expires (server unavailable), the daemon
# re-reconciles without the DHCP state and removes the address from the interface.
# Mapped to acceptance criteria:
#   "Lease expiry triggers reconciliation"
#   "The DHCP-acquired address is removed from eth0"
#
# NOTE: This test takes approximately 2-3 minutes because the minimum dnsmasq
#       lease time is 120 seconds.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-dhcp-lease-expiry.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lease-expiry: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Create a veth pair: veth-dhcp0 (client) / veth-dhcp1 (server side).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Start dnsmasq with a 120s lease (minimum dnsmasq lease time).
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

# Record the dnsmasq PID so we can kill it mid-test.
FIRST_DNSMASQ_PID="${_DNSMASQ_PIDS[0]}"

start_daemon

# Submit a DHCPv4 policy for veth-dhcp0.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: lease-expiry-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-dhcp-lease-expiry: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for the initial DHCP lease to appear (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.0." 10
assert_has_address veth-dhcp0 "10.99.0."

# Allow the daemon to finish post-lease reconciliation before killing the server.
sleep 3

# Kill dnsmasq with SIGKILL so it cannot send DHCPNAK (which would cause
# immediate address removal and race with our assertion). The lease must expire
# naturally after the server is gone.
kill -9 "$FIRST_DNSMASQ_PID" 2>/dev/null || true
_DNSMASQ_PIDS=()

# Wait for the lease to expire and the daemon to remove the address.
# 120s lease + 30s margin = 150s total.
wait_for_no_address veth-dhcp0 "10.99.0." 150

# Verify the address is gone: reconciliation ran without DHCP state.
assert_not_has_address veth-dhcp0 "10.99.0."

# Verify the daemon is still running (lease expiry must not crash the daemon).
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 403-dhcp-lease-expiry: daemon exited after lease expiry (expected still running)" >&2
    exit 1
fi

echo "PASS: 403-dhcp-lease-expiry"
