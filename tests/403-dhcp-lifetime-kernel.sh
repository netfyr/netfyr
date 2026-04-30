#!/bin/bash
# 403-dhcp-lifetime-kernel.sh
# Integration test: DHCP-acquired addresses must have finite kernel lifetimes
# (valid_lft and preferred_lft) instead of "forever".
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-dhcp-lifetime-kernel.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-dhcp-lifetime-kernel: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-dhcp-lifetime-kernel: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lifetime-kernel: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
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

# Create veth pair: veth-dhcp0 (client) / veth-dhcp1 (server).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Start dnsmasq DHCP server (minimum lease = 120s enforced by dnsmasq).
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

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
        echo "FAIL: 403-dhcp-lifetime-kernel: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-dhcp-lifetime-kernel: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a DHCPv4 policy for veth-dhcp0.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dhcp-lifetime-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"

# Wait for the DHCP lease to appear.
wait_for_address veth-dhcp0 "10.99.0." 10

# Allow a moment for the address to be fully applied with CacheInfo.
sleep 1

# Verify the DHCP address has a finite valid_lft (not "forever").
assert_valid_lft_finite veth-dhcp0 "10.99.0."

echo "PASS: 403-dhcp-lifetime-kernel"
