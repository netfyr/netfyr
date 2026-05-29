#!/bin/bash
# 356-show-json-waiting.sh -- End-to-end: netfyr show -o json omits lease fields for waiting factory.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 356-show-json-waiting: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 356-show-json-waiting: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 356-show-json-waiting: jq not found; install jq to run JSON validation tests" >&2
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

# Create veth pair — no DHCP server, factory will be in waiting state.
create_veth veth-dhcp0 veth-dhcp1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 356-show-json-waiting: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 356-show-json-waiting: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a DHCP policy for veth-dhcp0 (no DHCP server available).
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-show-jwait
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 356-show-json-waiting: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Poll for the DHCP factory to enter waiting state (up to 30 seconds).
WAIT_ITERS=0
JSON_OUTPUT=""
while true; do
    JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)
    DHCP_STATE=$(echo "$JSON_OUTPUT" | jq -r '.interfaces[] | select(.name == "veth-dhcp0") | .dhcp.state' 2>/dev/null || echo "")
    if [[ "$DHCP_STATE" == "waiting" ]]; then
        break
    fi
    if (( WAIT_ITERS >= 300 )); then
        echo "FAIL: 356-show-json-waiting: DHCP factory did not enter 'waiting' state within 30 seconds" >&2
        echo "      dhcp.state: $DHCP_STATE" >&2
        echo "      JSON: $JSON_OUTPUT" >&2
        exit 1
    fi
    sleep 0.1
    (( WAIT_ITERS++ )) || true
done

# Validate it is a JSON object.
if ! echo "$JSON_OUTPUT" | jq -e 'type == "object"' >/dev/null 2>&1; then
    echo "FAIL: 356-show-json-waiting: JSON output is not an object" >&2
    exit 1
fi

# Find veth-dhcp0 entry.
DHCP_IFACE=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-dhcp0")')
if [[ -z "$DHCP_IFACE" ]]; then
    echo "FAIL: 356-show-json-waiting: veth-dhcp0 not found in interfaces array" >&2
    echo "      JSON: $JSON_OUTPUT" >&2
    exit 1
fi

# Verify dhcp.state = "waiting".
DHCP_STATE=$(echo "$DHCP_IFACE" | jq -r '.dhcp.state')
if [[ "$DHCP_STATE" != "waiting" ]]; then
    echo "FAIL: 356-show-json-waiting: dhcp.state is '$DHCP_STATE', expected 'waiting'" >&2
    exit 1
fi

# Verify dhcp object does NOT have lease_time_secs.
HAS_LEASE_TIME=$(echo "$DHCP_IFACE" | jq '.dhcp | has("lease_time_secs")')
if [[ "$HAS_LEASE_TIME" != "false" ]]; then
    echo "FAIL: 356-show-json-waiting: dhcp object must not have 'lease_time_secs' for waiting factory" >&2
    echo "      dhcp object: $(echo "$DHCP_IFACE" | jq '.dhcp')" >&2
    exit 1
fi

# Verify dhcp object does NOT have lease_remaining_secs.
HAS_REMAINING=$(echo "$DHCP_IFACE" | jq '.dhcp | has("lease_remaining_secs")')
if [[ "$HAS_REMAINING" != "false" ]]; then
    echo "FAIL: 356-show-json-waiting: dhcp object must not have 'lease_remaining_secs' for waiting factory" >&2
    exit 1
fi

# Verify dhcp object does NOT have lease_address.
HAS_ADDR=$(echo "$DHCP_IFACE" | jq '.dhcp | has("lease_address")')
if [[ "$HAS_ADDR" != "false" ]]; then
    echo "FAIL: 356-show-json-waiting: dhcp object must not have 'lease_address' for waiting factory" >&2
    exit 1
fi

echo "PASS: 356-show-json-waiting"
