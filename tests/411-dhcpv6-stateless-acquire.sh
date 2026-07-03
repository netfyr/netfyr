#!/bin/bash
# 411-dhcpv6-stateless-acquire.sh
# Integration test: DHCPv6 stateless mode (Information-Request) acquires DNS
# servers (but no address) via the ipv6auto factory in an unprivileged
# user+network namespace.
#
# Requires: unshare, ip (iproute2), dnsmasq (with RA and DHCPv6 support)
#
# NOTE: This test exercises the DHCPv6 client through the ipv6auto factory.
# It requires SPEC-412 (ipv6auto → DHCPv6 integration) to be implemented
# before it can pass. The test is structurally complete and will pass once
# SPEC-412 wires up O-flag handling in the ipv6auto factory.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/411-dhcpv6-stateless-acquire.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 411-dhcpv6-stateless-acquire: dnsmasq not found; install dnsmasq to run DHCPv6 integration tests" >&2
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

# Start dnsmasq as RA sender with O flag (other-config) and DHCPv6 stateless server.
# --ra-param=veth-v6-1,other sets the O flag (not M), enabling stateless DHCPv6.
# --dhcp-range=::,ra-stateless configures dnsmasq for stateless mode only.
dnsmasq \
    --no-daemon \
    --interface=veth-v6-1 \
    --enable-ra \
    --ra-param=veth-v6-1,other \
    --dhcp-range=::,ra-stateless \
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
cat > "$POLICY_DIR/dhcpv6-stateless.yaml" <<'EOF'
kind: policy
name: dhcpv6-stateless-test
factory: ipv6auto
selector:
  name: veth-v6-0
EOF

# Start the daemon.
start_daemon

# Submit the policy.
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcpv6-stateless.yaml"

# Wait up to 30 seconds for the SLAAC address to appear (confirms the factory ran).
# In stateless mode, SLAAC addresses come from the RA prefix. The O flag also
# triggers an Information-Request to get DNS servers.
wait_for_address veth-v6-0 "2001:db8::" 30

# Verify DNS servers appear in the daemon query output.
QUERY_OUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query --selector name=veth-v6-0 2>&1) || true
if ! echo "$QUERY_OUT" | grep -q "dns_servers"; then
    echo "FAIL: 411-dhcpv6-stateless-acquire: dns_servers not found in query output" >&2
    echo "      netfyr query output:" >&2
    echo "$QUERY_OUT" >&2
    exit 1
fi

# Verify no DHCPv6-assigned /128 address exists (stateless mode assigns no addresses
# via DHCPv6; only SLAAC /64 addresses should be present).
ADDR_OUT=$(ip -6 addr show dev veth-v6-0 2>&1)
if echo "$ADDR_OUT" | grep "2001:db8::" | grep -q "/128"; then
    echo "FAIL: 411-dhcpv6-stateless-acquire: unexpected /128 DHCPv6 address found (stateless mode)" >&2
    echo "      ip -6 addr show veth-v6-0:" >&2
    echo "$ADDR_OUT" >&2
    exit 1
fi

echo "PASS: 411-dhcpv6-stateless-acquire"
