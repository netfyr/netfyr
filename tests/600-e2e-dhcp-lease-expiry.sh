#!/bin/bash
# 600-e2e-dhcp-lease-expiry.sh -- End-to-end: DHCP address removed on lease expiry and re-acquired when server returns.
#
# NOTE: This test takes approximately 2-3 minutes because the minimum dnsmasq
#       lease time is 120 seconds.
#
# Requires: unshare, ip (iproute2), dnsmasq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-lease-expiry: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-lease-expiry: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-lease-expiry: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# Set up veth pair: veth-dhcp0 is the client side, veth-dhcp1 is the server side.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24

# Start dnsmasq with a 120s lease (minimum dnsmasq lease time).
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

# Record the dnsmasq PID so we can kill it mid-test without affecting the EXIT trap.
FIRST_DNSMASQ_PID="${_DNSMASQ_PIDS[0]}"

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-dhcp-lease-expiry: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-dhcp-lease-expiry: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write DHCP policy for veth-dhcp0.
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-lease-expiry
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-lease-expiry: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for the DHCP lease to appear (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10
assert_has_address veth-dhcp0 "10.99.1."

# Allow the daemon to complete all post-lease reconciliation cycles before we
# start watching for the address to disappear.  Without this pause, a second
# reconcile triggered by external-change detection of the daemon's own address
# addition can cause a sub-second remove/re-add that fools wait_for_no_address.
sleep 3

# Kill dnsmasq so renewal and rebind attempts fail, causing the lease to expire.
# Use SIGKILL (not SIGTERM) to prevent dnsmasq from sending a DHCPNAK on shutdown:
# a DHCPNAK causes the client to immediately remove the address and restart
# discovery, which would race with the post-kill assertion.
# Reset the array so the EXIT trap doesn't try to kill the now-dead process.
kill -9 "$FIRST_DNSMASQ_PID" 2>/dev/null || true
_DNSMASQ_PIDS=()

# Wait for the lease to expire and the address to be removed (120s lease + 30s margin).
wait_for_no_address veth-dhcp0 "10.99.1." 150
assert_not_has_address veth-dhcp0 "10.99.1."

# Restart dnsmasq with the same configuration.
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

# Wait for the client to re-acquire a lease (up to 30 seconds).
wait_for_address veth-dhcp0 "10.99.1." 30
assert_has_address veth-dhcp0 "10.99.1."

echo "PASS: 600-e2e-dhcp-lease-expiry"
