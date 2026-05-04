#!/bin/bash
# 600-e2e-query-selector-daemon.sh
# Integration test: Verify that query selectors filter results in daemon mode.
# Reproduces bug 001: selectors (name, mac, driver) were silently ignored when
# querying via Varlink, returning all interfaces instead of filtering.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-query-selector-daemon.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-query-selector-daemon: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-query-selector-daemon: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-query-selector-daemon: jq not found" >&2
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

# Create two veth pairs so we have multiple interfaces to filter among.
create_veth veth-a0 veth-a1
create_veth veth-b0 veth-b1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-query-selector-daemon: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-query-selector-daemon: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Test 1: name selector returns only the matching interface ────────────

output=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query -s name=veth-a0 -o json)
count=$(echo "$output" | jq 'length')

if [[ "$count" -ne 1 ]]; then
    echo "FAIL: 600-e2e-query-selector-daemon: name=veth-a0 returned $count entities, expected 1" >&2
    echo "Output: $output" >&2
    exit 1
fi

returned_name=$(echo "$output" | jq -r '.[0].name')
if [[ "$returned_name" != "veth-a0" ]]; then
    echo "FAIL: 600-e2e-query-selector-daemon: expected name 'veth-a0', got '$returned_name'" >&2
    exit 1
fi

# ── Test 2: nonexistent name returns empty result ────────────────────────

output=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query -s name=nonexistent -o json)
count=$(echo "$output" | jq 'length')

if [[ "$count" -ne 0 ]]; then
    echo "FAIL: 600-e2e-query-selector-daemon: name=nonexistent returned $count entities, expected 0" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 600-e2e-query-selector-daemon"
