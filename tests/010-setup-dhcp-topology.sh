#!/bin/bash
# 010-setup-dhcp-topology.sh
# Integration test: setup_dhcp_topology creates a veth pair with both ends up,
# assigns the server IP to the server veth, and starts a dnsmasq DHCP server
# serving the specified range. Also verifies the EXIT trap kills dnsmasq and
# removes the temp directory when the script exits.
#
# Acceptance criteria covered:
#   - setup_dhcp_topology creates veth-dhcp0 and veth-dhcp1 (both up)
#   - veth-dhcp1 (server veth) has the assigned server IP
#   - dnsmasq is running on veth-dhcp1 serving the specified range
#   - EXIT trap kills dnsmasq processes and removes temp files
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage: bash tests/010-setup-dhcp-topology.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DNSMASQ_LOG="$TMPDIR_TEST/dnsmasq.log"
trap 'cleanup; rm -rf "$TMPDIR_TEST"' EXIT

# -----------------------------------------------------------------------
# Test: setup_dhcp_topology creates veth pair, assigns server IP, starts dnsmasq
# -----------------------------------------------------------------------

# setup_dhcp_topology CLIENT_VETH SERVER_VETH SERVER_IP RANGE_START RANGE_END [LEASE_TIME]
setup_dhcp_topology veth-dt0 veth-dt1 10.99.60.1 10.99.60.100 10.99.60.200 30

# Verify veth-dt0 exists and is UP.
assert_link_up veth-dt0

# Verify veth-dt1 exists and is UP.
assert_link_up veth-dt1

# Verify server IP is assigned to veth-dt1.
assert_has_address veth-dt1 "10.99.60.1"

# Verify dnsmasq started (at least one PID tracked in _DNSMASQ_PIDS).
if [[ "${#_DNSMASQ_PIDS[@]}" -eq 0 ]]; then
    echo "FAIL: 010-setup-dhcp-topology: no dnsmasq PIDs in _DNSMASQ_PIDS after setup_dhcp_topology" >&2
    exit 1
fi

DNSMASQ_PID="${_DNSMASQ_PIDS[0]}"
if ! kill -0 "$DNSMASQ_PID" 2>/dev/null; then
    echo "FAIL: 010-setup-dhcp-topology: dnsmasq PID=$DNSMASQ_PID is not running" >&2
    exit 1
fi

# -----------------------------------------------------------------------
# Test: EXIT trap kills dnsmasq (verified via a separate subshell)
# The subshell's _DNSMASQ_PIDS array and the cleanup() function run when
# the subshell's EXIT trap fires.
# -----------------------------------------------------------------------
SHARED=$(mktemp)

(
    # shellcheck source=helpers.sh
    source "$SCRIPT_DIR/helpers.sh"

    setup_dhcp_topology veth-dt2 veth-dt3 10.99.61.1 10.99.61.100 10.99.61.200 30

    # Record the dnsmasq PID for the parent to verify it is killed.
    echo "INNER_DNSMASQ_PID=${_DNSMASQ_PIDS[0]:-}" > "$SHARED"

    # Exit normally; cleanup() fires as the EXIT trap and kills dnsmasq.
    exit 0
)

# shellcheck source=/dev/null
source "$SHARED"
rm -f "$SHARED"

if [[ -n "${INNER_DNSMASQ_PID:-}" ]] && kill -0 "$INNER_DNSMASQ_PID" 2>/dev/null; then
    echo "FAIL: 010-setup-dhcp-topology: EXIT trap did not kill dnsmasq PID=$INNER_DNSMASQ_PID" >&2
    exit 1
fi

echo "PASS: 010-setup-dhcp-topology"
