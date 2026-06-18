#!/bin/bash
# 600-e2e-dhcp-and-static.sh -- Cross-cutting: DHCP and static policies coexist
# on separate interfaces without mutual interference.
#
# Exercises: static policy reconciliation, DHCP factory, journal, history CLI.
#
# Scenario:
#   - veth-static0/veth-static1: statically configured (mtu=1400, 10.99.0.1/24)
#   - veth-dhcp0/veth-dhcp1: DHCP-configured; dnsmasq on veth-dhcp1 serves
#     addresses in 10.99.1.100-10.99.1.200
#   - Both policies are applied simultaneously; the test verifies that each
#     interface gets only its own configuration and that the journal records a
#     dhcp-acquire entry.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-dhcp-and-static.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-and-static: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
setup_journal

# Static interface pair: veth-static0 (managed), veth-static1 (peer).
create_veth veth-static0 veth-static1

# DHCP interface pair: veth-dhcp0 (managed client), veth-dhcp1 (server peer).
# Assign the server IP and start dnsmasq; PIDs are tracked by _daemon_test_cleanup.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

start_daemon

# ── Write and apply policies ─────────────────────────────────────────────────

APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/static.yaml" <<'EOF'
kind: policy
name: e2e-ds-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-static0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-ds-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply both policies atomically from the directory.
APPLY_EXIT=0
"$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-and-static: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Wait for DHCP lease (up to 10 s) ────────────────────────────────────────

wait_for_address veth-dhcp0 "10.99.1." 10

# ── Verify static interface: mtu=1400 and address 10.99.0.1/24 ───────────────

assert_mtu veth-static0 1400
assert_has_address veth-static0 "10.99.0.1"

# ── Verify DHCP interface: address in the 10.99.1.0/24 range ────────────────

assert_has_address veth-dhcp0 "10.99.1."

# ── Verify no cross-interface interference ───────────────────────────────────

# The static address must not appear on the DHCP interface.
assert_not_has_address veth-dhcp0 "10.99.0."

# The DHCP-acquired address range must not appear on the static interface.
assert_not_has_address veth-static0 "10.99.1."

# The static policy's MTU (1400) must not have bled onto the DHCP interface.
DHCP_LINK_OUTPUT=$(ip link show veth-dhcp0 2>&1)
if echo "$DHCP_LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 600-e2e-dhcp-and-static: veth-dhcp0 unexpectedly has mtu 1400 (static MTU bled onto DHCP interface)" >&2
    echo "      ip link output: $DHCP_LINK_OUTPUT" >&2
    exit 1
fi

# ── Verify history shows a dhcp-acquire entry ────────────────────────────────

# Give the daemon time to finish writing the journal entry after applying the
# DHCP state to netlink.
sleep 2

HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 5 2>&1)
if ! echo "$HISTORY_OUTPUT" | grep -qF "dhcp-acquire"; then
    echo "FAIL: 600-e2e-dhcp-and-static: netfyr history -n 5 does not contain 'dhcp-acquire' in TRIGGER column" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-dhcp-and-static"
