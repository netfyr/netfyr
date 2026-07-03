#!/bin/bash
# 600-e2e-dual-stack.sh -- Cross-cutting: DHCPv4 and ipv6auto (SLAAC) coexist on
# the same interface, producing merged ipv4 and ipv6 state.
#
# Exercises: DHCPv4 factory, ipv6auto factory, reconciler merge of two factory
# states for a single interface, backend apply, journal, history CLI.
#
# Scenario:
#   - veth-ds0/veth-ds1: veth pair; daemon manages veth-ds0
#   - veth-ds1: server end with 10.99.1.1/24 (DHCPv4) and 2001:db8::1/64 (RA)
#   - dnsmasq: serves DHCPv4 (10.99.1.100-200) and RAs for SLAAC (2001:db8::/64)
#   - Two policies: dhcpv4 and ipv6auto for veth-ds0
#   - After apply: veth-ds0 has both IPv4 (from DHCPv4) and IPv6 (from SLAAC)
#
# Requires: unshare, ip (iproute2), dnsmasq (with RA support)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-dual-stack.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dual-stack: dnsmasq not found; install dnsmasq to run dual-stack tests" >&2
    exit 1
fi

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
setup_journal

# Create veth pair: veth-ds0 (managed client) / veth-ds1 (server peer).
create_veth veth-ds0 veth-ds1

# Assign server-side addresses: IPv4 for DHCPv4, IPv6 for RA prefix.
add_address veth-ds1 10.99.1.1/24
ip addr add 2001:db8::1/64 dev veth-ds1

# Start a single dnsmasq process serving both DHCPv4 and IPv6 RAs (SLAAC only,
# no DHCPv6 address assignment).  dnsmasq infers the RA prefix from veth-ds1's
# own 2001:db8::/64 address.
dnsmasq \
    --no-daemon \
    --bind-dynamic \
    --interface=veth-ds1 \
    --dhcp-range=10.99.1.100,10.99.1.200,120 \
    --enable-ra \
    --dhcp-range=::,ra-only \
    --no-resolv \
    --no-hosts \
    --log-dhcp \
    >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

start_daemon

# ── Write and apply policies ─────────────────────────────────────────────────

APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/dhcpv4.yaml" <<'EOF'
kind: policy
name: e2e-ds-dhcpv4
factory: dhcpv4
selector:
  name: veth-ds0
EOF

cat > "$APPLY_DIR/ipv6auto.yaml" <<'EOF'
kind: policy
name: e2e-ds-ipv6auto
factory: ipv6auto
selector:
  name: veth-ds0
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-dual-stack: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Wait for addresses ────────────────────────────────────────────────────────

# DHCPv4: typically acquired within a few seconds.
wait_for_address veth-ds0 "10.99.1." 15

# SLAAC: link-local DAD (~1s) + RA receipt (triggered by RS) + global DAD (~1s).
wait_for_address veth-ds0 "2001:db8::" 30

# ── Assertions ────────────────────────────────────────────────────────────────

assert_has_address veth-ds0 "10.99.1."
assert_has_address veth-ds0 "2001:db8::"
assert_has_address veth-ds0 "fe80:"

# ── JSON query verification ───────────────────────────────────────────────────

QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query --selector name=veth-ds0 --output json 2>&1)

if ! echo "$QUERY_OUTPUT" | grep -q '"ipv4"'; then
    echo "FAIL: 600-e2e-dual-stack: netfyr query JSON output does not contain 'ipv4'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"ipv6"'; then
    echo "FAIL: 600-e2e-dual-stack: netfyr query JSON output does not contain 'ipv6'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"addresses"'; then
    echo "FAIL: 600-e2e-dual-stack: netfyr query JSON output does not contain 'addresses'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

# ── History verification ──────────────────────────────────────────────────────

# Give the daemon time to finish writing journal entries after applying state.
sleep 2

HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 10 2>&1)
if ! echo "$HISTORY_OUTPUT" | grep -qF "dhcp-acquire"; then
    echo "FAIL: 600-e2e-dual-stack: netfyr history -n 10 does not contain 'dhcp-acquire'" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-dual-stack"
