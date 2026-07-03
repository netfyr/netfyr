#!/bin/bash
# 600-e2e-ipv6auto-dhcpv6.sh -- Cross-cutting: ipv6auto with RA O flag acquires
# SLAAC addresses and DHCPv6 stateless DNS, merged into a single ipv6 sub-object.
#
# Exercises: ipv6auto factory, SLAAC address acquisition, DHCPv6 stateless mode
# (O flag triggered by RA), daemon apply → query path, produced state format.
#
# Scenario:
#   - veth-v6-0/veth-v6-1: veth pair; daemon manages veth-v6-0
#   - veth-v6-1: server end with 2001:db8::1/64
#   - dnsmasq: RA with O flag + DHCPv6 stateless DNS (2001:db8::53)
#   - One policy: ipv6auto for veth-v6-0
#   - After apply: veth-v6-0 has SLAAC address and DHCPv6 DNS in ipv6 sub-object;
#     no /128 DHCPv6 address (stateless mode assigns no addresses)
#
# Requires: unshare, ip (iproute2), dnsmasq (with RA and DHCPv6 support)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-ipv6auto-dhcpv6.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: dnsmasq not found; install dnsmasq to run DHCPv6 tests" >&2
    exit 1
fi

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
setup_journal

# Create veth pair: veth-v6-0 (managed client) / veth-v6-1 (server peer).
create_veth veth-v6-0 veth-v6-1

# Assign a routable IPv6 address to veth-v6-1 so dnsmasq can serve as a router
# and advertise the 2001:db8::/64 prefix.
ip addr add 2001:db8::1/64 dev veth-v6-1

# Start dnsmasq with RA (O flag) and DHCPv6 stateless mode.
# --ra-param=veth-v6-1,other: sets the O flag (not M) in RA messages, signalling
#   that clients should send DHCPv6 Information-Request for config (no addresses).
# --dhcp-range=::,ra-stateless: stateless DHCPv6 only (responds to Info-Request).
# --dhcp-option=option6:dns-server: provides DNS servers via DHCPv6 stateless.
dnsmasq \
    --no-daemon \
    --interface=veth-v6-1 \
    --enable-ra \
    --ra-param=veth-v6-1,other \
    --dhcp-range=::,ra-stateless \
    --dhcp-option=option6:dns-server,[2001:db8::53] \
    --bind-dynamic \
    --no-resolv \
    --no-hosts \
    --log-dhcp \
    >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# Write ipv6auto policy for veth-v6-0.
cat > "$POLICY_DIR/ipv6auto.yaml" <<'EOF'
kind: policy
name: e2e-v6-ipv6auto
factory: ipv6auto
selector:
  name: veth-v6-0
EOF

start_daemon

# Apply the policy.
APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_DIR/ipv6auto.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Wait for SLAAC address ────────────────────────────────────────────────────

# SLAAC: link-local DAD (~1s) + RA receipt (triggered by RS) + global DAD (~1s).
wait_for_address veth-v6-0 "2001:db8::" 30

# ── Verify SLAAC address is not tentative ────────────────────────────────────

ADDR_OUT=$(ip -6 addr show dev veth-v6-0 2>&1)
if ! echo "$ADDR_OUT" | grep "2001:db8::" | grep -qv "tentative"; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: SLAAC address is still tentative" >&2
    echo "      ip -6 addr show veth-v6-0:" >&2
    echo "$ADDR_OUT" >&2
    exit 1
fi

# ── Verify no DHCPv6-assigned /128 address ───────────────────────────────────

if echo "$ADDR_OUT" | grep "2001:db8::" | grep -q "/128"; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: unexpected /128 DHCPv6 address (stateless mode assigns no addresses)" >&2
    echo "      ip -6 addr show veth-v6-0:" >&2
    echo "$ADDR_OUT" >&2
    exit 1
fi

# ── JSON query verification ───────────────────────────────────────────────────

QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query --selector name=veth-v6-0 --output json 2>&1)

if ! echo "$QUERY_OUTPUT" | grep -q '"dns_servers"'; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: netfyr query JSON output does not contain 'dns_servers'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q "2001:db8::53"; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: netfyr query JSON output does not contain DNS server 2001:db8::53" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"addresses"'; then
    echo "FAIL: 600-e2e-ipv6auto-dhcpv6: netfyr query JSON output does not contain 'addresses'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-ipv6auto-dhcpv6"
