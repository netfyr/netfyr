#!/bin/bash
# 302-daemon-query-selector.sh -- Daemon mode: query with selector delegates
# to daemon via Varlink.
#
# Scenario 58: Creates a veth pair with a non-default MTU, starts the daemon,
# runs `netfyr query -s name=...` which routes through Varlink, and verifies
# the output contains the correct interface name and MTU.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/302-daemon-query-selector.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-test0 veth-test1
ip link set veth-test0 mtu 1400

start_daemon

QUERY_EXIT=0
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-test0 -o json 2>&1) || QUERY_EXIT=$?

if [[ $QUERY_EXIT -ne 0 ]]; then
    echo "FAIL: 302-daemon-query-selector: query exited with $QUERY_EXIT" >&2
    echo "      output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"veth-test0"'; then
    echo "FAIL: 302-daemon-query-selector: output does not contain \"veth-test0\"" >&2
    echo "      output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '"mtu".*1400'; then
    echo "FAIL: 302-daemon-query-selector: output does not show mtu=1400" >&2
    echo "      output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 302-daemon-query-selector"
