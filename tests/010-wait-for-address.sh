#!/bin/bash
# 010-wait-for-address.sh
# Integration test: wait_for_address returns 0 when an address matching the
# pattern appears, exits 1 on timeout; wait_for_no_address returns 0 when a
# matching address disappears, exits 1 on timeout.
#
# Acceptance criteria covered:
#   - wait_for_address returns 0 when matching address appears within timeout
#   - wait_for_address exits 1 on timeout when no matching address appears
#   - wait_for_no_address returns 0 when matching address disappears within timeout
#
# Requires: unshare, ip (iproute2)
# Usage: bash tests/010-wait-for-address.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"

# ---------- Inside the namespace ----------

# Use a temp dir for cleanup marker (no daemon, so set up minimal trap).
TMPDIR_TEST=$(mktemp -d)
trap 'cleanup; rm -rf "$TMPDIR_TEST"' EXIT

# -----------------------------------------------------------------------
# Test: wait_for_address returns 0 when address appears within timeout
# -----------------------------------------------------------------------
create_veth veth-wa0 veth-wa1

# Add address after a 200ms delay from a background subshell.
(sleep 0.2 && ip addr add 10.99.50.5/24 dev veth-wa0) &

wait_for_address veth-wa0 "10.99.50." 10
if [[ $? -ne 0 ]]; then
    echo "FAIL: 010-wait-for-address: wait_for_address should return 0 when address appears" >&2
    exit 1
fi

# -----------------------------------------------------------------------
# Test: wait_for_address exits 1 when timeout expires (no address added)
# -----------------------------------------------------------------------
create_veth veth-wa2 veth-wa3

subshell_rc=0
(
    wait_for_address veth-wa2 "10.99.51." 1 2>/dev/null
) 2>/dev/null || subshell_rc=$?

if [[ "$subshell_rc" -ne 1 ]]; then
    echo "FAIL: 010-wait-for-address: wait_for_address should exit 1 on timeout, got $subshell_rc" >&2
    exit 1
fi

# -----------------------------------------------------------------------
# Test: wait_for_no_address returns 0 when address disappears within timeout
# -----------------------------------------------------------------------
create_veth veth-wna0 veth-wna1
ip addr add 10.99.52.5/24 dev veth-wna0

# Remove the address after a 200ms delay.
(sleep 0.2 && ip addr del 10.99.52.5/24 dev veth-wna0) &

wait_for_no_address veth-wna0 "10.99.52." 10
if [[ $? -ne 0 ]]; then
    echo "FAIL: 010-wait-for-address: wait_for_no_address should return 0 when address disappears" >&2
    exit 1
fi

# -----------------------------------------------------------------------
# Test: wait_for_no_address exits 1 when timeout expires (address stays)
# -----------------------------------------------------------------------
create_veth veth-wna2 veth-wna3
ip addr add 10.99.53.5/24 dev veth-wna2

subshell_rc=0
(
    wait_for_no_address veth-wna2 "10.99.53." 1 2>/dev/null
) 2>/dev/null || subshell_rc=$?

if [[ "$subshell_rc" -ne 1 ]]; then
    echo "FAIL: 010-wait-for-address: wait_for_no_address should exit 1 on timeout, got $subshell_rc" >&2
    exit 1
fi

echo "PASS: 010-wait-for-address"
