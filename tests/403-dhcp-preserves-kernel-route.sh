#!/bin/bash
# 403-dhcp-preserves-kernel-route.sh
# Integration test: When a DHCP policy is applied through the daemon, the
# kernel-generated prefix route for the leased address must not be deleted.
# Mapped to spec: "DHCP apply must not remove proto-kernel routes".
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-dhcp-preserves-kernel-route.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create DHCP veth pair: veth-dhcp0 (client) / veth-dhcp1 (server side).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.60.1/24

# Start dnsmasq with gateway option so the DHCP lease includes a default route.
# --dhcp-option=3,<gateway> sends option 3 (router) to clients.
dnsmasq \
    --no-daemon \
    --bind-dynamic \
    --interface=veth-dhcp1 \
    --dhcp-range=10.99.60.100,10.99.60.200,120 \
    --dhcp-option=3,10.99.60.1 \
    --dhcp-leasefile="$TMPDIR_TEST/leases" \
    --no-resolv \
    --no-hosts \
    --log-dhcp \
    >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket.
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 403-dhcp-preserves-kernel-route: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-dhcp-preserves-kernel-route: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit a DHCPv4 policy for veth-dhcp0.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: veth-dhcp0-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"
APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for the DHCP lease to be acquired.
wait_for_address veth-dhcp0 "10.99.60." 10

# Give the daemon a moment to reconcile after lease acquisition.
sleep 1

# Extract the leased address prefix (e.g. "10.99.60.0/24").
LEASED_IP=$(ip addr show dev veth-dhcp0 2>/dev/null \
    | grep -oP 'inet \K[0-9.]+(?=/)' | head -1)
if [[ -z "$LEASED_IP" ]]; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: could not determine leased IP" >&2
    exit 1
fi

# The kernel prefix route must still exist for the leased subnet.
ROUTES=$(ip route)
if ! echo "$ROUTES" | grep -q "10.99.60.0/24"; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: kernel prefix route 10.99.60.0/24 was deleted" >&2
    echo "      ip route: $ROUTES" >&2
    exit 1
fi

# The default route (from DHCP gateway option) should also exist.
if ! echo "$ROUTES" | grep -q "default"; then
    echo "FAIL: 403-dhcp-preserves-kernel-route: default route not found after DHCP lease" >&2
    echo "      ip route: $ROUTES" >&2
    exit 1
fi

echo "PASS: 403-dhcp-preserves-kernel-route"
