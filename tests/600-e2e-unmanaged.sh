#!/bin/bash
# 600-e2e-unmanaged.sh -- End-to-end: interfaces without policies are not modified.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-unmanaged.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-unmanaged: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-unmanaged: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-unmanaged: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Three interface pairs: managed (static), unmanaged, and managed (DHCP).
create_veth veth-managed0 veth-managed1
create_veth veth-other0 veth-other1
create_veth veth-dhcp0 veth-dhcp1

# Manually configure the unmanaged interface — this must survive the policy apply.
ip link set dev veth-other0 mtu 1400
add_address veth-other0 10.99.2.1/24

# Configure the DHCP server side.
add_address veth-dhcp1 10.99.3.1/24

# Start dnsmasq DHCP server on the server-side interface.
start_dnsmasq veth-dhcp1 10.99.3.1 10.99.3.100 10.99.3.200 120

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-unmanaged: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-unmanaged: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write policies only for veth-managed0 (static) and veth-dhcp0 (DHCPv4).
# No policy for veth-other0.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/static.yaml" <<'EOF'
kind: policy
name: e2e-unmanaged-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-managed0
  mtu: 1300
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-unmanaged-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-unmanaged: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for the DHCP lease on the managed DHCP interface.
wait_for_address veth-dhcp0 "10.99.3." 10

# Verify managed interfaces are configured correctly.
assert_mtu veth-managed0 1300
assert_has_address veth-dhcp0 "10.99.3."

# Verify the unmanaged interface is completely untouched.
assert_mtu veth-other0 1400
assert_has_address veth-other0 "10.99.2.1"

echo "PASS: 600-e2e-unmanaged"
