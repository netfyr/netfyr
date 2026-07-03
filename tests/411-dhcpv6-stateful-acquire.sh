#!/bin/bash
# 411-dhcpv6-stateful-acquire.sh
# Integration test: DHCPv6 stateful mode (IA_NA) acquires an address and DNS
# servers via the ipv6auto factory in an unprivileged user+network namespace.
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
#   bash tests/411-dhcpv6-stateful-acquire.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 411-dhcpv6-stateful-acquire: dnsmasq not found; install dnsmasq to run DHCPv6 integration tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Create veth pair: veth-v6-0 (client/daemon) / veth-v6-1 (RA + DHCPv6 server).
create_veth veth-v6-0 veth-v6-1

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

# Write ipv6auto policy for veth-v6-0.
cat > "$POLICY_DIR/dhcpv6-stateful.yaml" <<'EOF'
kind: policy
name: dhcpv6-stateful-test
factory: ipv6auto
selector:
  name: veth-v6-0
EOF

# Start the daemon.
start_daemon

# Submit the policy.
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcpv6-stateful.yaml"

# Wait up to 30 seconds for a DHCPv6-assigned address in the 2001:db8:: prefix.
# DHCPv6 stateful: link-local DAD (~1s) + RA receipt + SOLICIT/ADVERTISE/REQUEST/REPLY.
wait_for_address veth-v6-0 "2001:db8::" 30

# Verify the address is not tentative.
ADDR_OUT=$(ip -6 addr show dev veth-v6-0 2>&1)
if ! echo "$ADDR_OUT" | grep "2001:db8::" | grep -qv "tentative"; then
    echo "FAIL: 411-dhcpv6-stateful-acquire: DHCPv6 address is still tentative" >&2
    echo "      ip -6 addr show veth-v6-0:" >&2
    echo "$ADDR_OUT" >&2
    exit 1
fi

# Verify DNS servers appear in the daemon query output.
QUERY_OUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query --selector name=veth-v6-0 2>&1) || true
if ! echo "$QUERY_OUT" | grep -q "dns_servers"; then
    echo "FAIL: 411-dhcpv6-stateful-acquire: dns_servers not found in query output" >&2
    echo "      netfyr query output:" >&2
    echo "$QUERY_OUT" >&2
    exit 1
fi

echo "PASS: 411-dhcpv6-stateful-acquire"
