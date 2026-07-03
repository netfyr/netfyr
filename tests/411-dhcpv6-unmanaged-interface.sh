#!/bin/bash
# 411-dhcpv6-unmanaged-interface.sh
# Integration test: An ipv6auto policy for one interface does not disturb
# another unmanaged interface's configuration when DHCPv6 stateful mode is
# active.
#
# Requires: unshare, ip (iproute2), dnsmasq (with RA and DHCPv6 support)
#
# NOTE: This test exercises the DHCPv6 client through the ipv6auto factory.
# It requires SPEC-412 (ipv6auto → DHCPv6 integration) to be implemented
# before it can pass. The test is structurally complete and will pass once
# SPEC-412 wires up M-flag handling in the ipv6auto factory.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/411-dhcpv6-unmanaged-interface.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 411-dhcpv6-unmanaged-interface: dnsmasq not found; install dnsmasq to run DHCPv6 integration tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Create managed veth pair: veth-v6-0 (client/daemon) / veth-v6-1 (RA + DHCPv6 server).
create_veth veth-v6-0 veth-v6-1

# Create unmanaged veth pair with a custom MTU and a static IPv6 address.
# The daemon must not touch this pair; we use the MTU as a sentinel.
create_veth veth-other0 veth-other1
ip link set dev veth-other0 mtu 1400
ip addr add 2001:db8:ff::1/128 dev veth-other0

# Assign a routable IPv6 address to veth-v6-1 so dnsmasq can serve as a router.
ip addr add 2001:db8::1/64 dev veth-v6-1

# Start dnsmasq as RA sender with M flag (managed) and DHCPv6 stateful server.
# --ra-param=veth-v6-1,managed sets the M flag in Router Advertisements.
# --dhcp-range=::100,::200,... enables DHCPv6 IA_NA address assignment.
dnsmasq \
    --no-daemon \
    --interface=veth-v6-1 \
    --enable-ra \
    --ra-param=veth-v6-1,managed \
    --dhcp-range=::100,::200,constructor:veth-v6-1,64,86400 \
    --dhcp-option=option6:dns-server,[2001:db8::53] \
    --dhcp-option=option6:domain-search,example.com \
    --bind-dynamic \
    --no-resolv \
    --no-hosts \
    --log-dhcp \
    >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)

# Brief pause for dnsmasq to start and bind.
sleep 1

# Write ipv6auto policy for veth-v6-0 ONLY — no policy for veth-other0.
cat > "$POLICY_DIR/dhcpv6-stateful.yaml" <<'EOF'
kind: policy
name: dhcpv6-unmanaged-test
factory: ipv6auto
selector:
  name: veth-v6-0
EOF

# Start the daemon.
start_daemon

# Submit the policy.
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcpv6-stateful.yaml"

# Wait up to 30 seconds for a DHCPv6-assigned address in the 2001:db8:: prefix
# on the managed interface. This confirms the ipv6auto factory ran and completed
# DHCPv6 negotiation (requires SPEC-412).
wait_for_address veth-v6-0 "2001:db8::" 30

# Verify the unmanaged interface has the same MTU (daemon must not have changed it).
ACTUAL_MTU=$(ip link show dev veth-other0 2>/dev/null | grep -oP 'mtu \K[0-9]+' || echo "unknown")
if [[ "$ACTUAL_MTU" != "1400" ]]; then
    echo "FAIL: 411-dhcpv6-unmanaged-interface: veth-other0 mtu changed from 1400 to $ACTUAL_MTU (daemon must not touch unmanaged interfaces)" >&2
    ip link show dev veth-other0 >&2 || true
    exit 1
fi

# Verify the static address on the unmanaged interface is still present.
assert_has_address veth-other0 "2001:db8:ff::1"

# Verify the unmanaged interface is still UP.
assert_link_up veth-other0

echo "PASS: 411-dhcpv6-unmanaged-interface"
