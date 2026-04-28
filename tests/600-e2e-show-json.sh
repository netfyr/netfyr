#!/bin/bash
# 600-e2e-show-json.sh -- End-to-end: netfyr show -o json produces valid JSON with DHCP lease data.
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-json: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-json: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json: jq not found; install jq to run JSON validation tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create veth pair: veth-dhcp0 is the client, veth-dhcp1 is the server.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24

# Start dnsmasq with a 120s lease.
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-json: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-json: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a DHCP policy for veth-dhcp0.
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-show-json
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-json: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease to be acquired (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# Run netfyr show -o json and capture output.
JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)

# Validate it is a JSON object.
if ! echo "$JSON_OUTPUT" | jq . >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json: show -o json output is not valid JSON" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi
if ! echo "$JSON_OUTPUT" | jq -e 'type == "object"' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json: JSON output is not an object" >&2
    exit 1
fi

# Verify daemon.status = "running".
DAEMON_STATUS=$(echo "$JSON_OUTPUT" | jq -r '.daemon.status')
if [[ "$DAEMON_STATUS" != "running" ]]; then
    echo "FAIL: 600-e2e-show-json: daemon.status is '$DAEMON_STATUS', expected 'running'" >&2
    exit 1
fi

# Verify daemon.uptime_seconds is a non-negative integer.
UPTIME=$(echo "$JSON_OUTPUT" | jq '.daemon.uptime_seconds')
if [[ "$UPTIME" == "null" ]] || [[ "$UPTIME" -lt 0 ]]; then
    echo "FAIL: 600-e2e-show-json: daemon.uptime_seconds is '$UPTIME', expected non-negative integer" >&2
    exit 1
fi

# Verify interfaces is an array with at least 1 element.
IFACE_COUNT=$(echo "$JSON_OUTPUT" | jq '.interfaces | length')
if [[ "$IFACE_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-show-json: interfaces array is empty" >&2
    exit 1
fi

# Find veth-dhcp0 entry.
DHCP_IFACE=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-dhcp0")')
if [[ -z "$DHCP_IFACE" ]]; then
    echo "FAIL: 600-e2e-show-json: veth-dhcp0 not found in interfaces array" >&2
    echo "      JSON: $JSON_OUTPUT" >&2
    exit 1
fi

# Verify policies array contains e2e-show-json with type dhcpv4.
POLICY_NAME=$(echo "$DHCP_IFACE" | jq -r '.policies[] | select(.name == "e2e-show-json") | .name')
POLICY_TYPE=$(echo "$DHCP_IFACE" | jq -r '.policies[] | select(.name == "e2e-show-json") | .type')
if [[ "$POLICY_NAME" != "e2e-show-json" ]]; then
    echo "FAIL: 600-e2e-show-json: policy 'e2e-show-json' not found in veth-dhcp0 policies" >&2
    exit 1
fi
if [[ "$POLICY_TYPE" != "dhcpv4" ]]; then
    echo "FAIL: 600-e2e-show-json: policy type is '$POLICY_TYPE', expected 'dhcpv4'" >&2
    exit 1
fi

# Verify dhcp.state = "running".
DHCP_STATE=$(echo "$DHCP_IFACE" | jq -r '.dhcp.state')
if [[ "$DHCP_STATE" != "running" ]]; then
    echo "FAIL: 600-e2e-show-json: dhcp.state is '$DHCP_STATE', expected 'running'" >&2
    exit 1
fi

# Verify dhcp.lease_address contains "10.99.1." and "/".
LEASE_ADDR=$(echo "$DHCP_IFACE" | jq -r '.dhcp.lease_address')
if ! echo "$LEASE_ADDR" | grep -q "10.99.1."; then
    echo "FAIL: 600-e2e-show-json: dhcp.lease_address '$LEASE_ADDR' does not contain '10.99.1.'" >&2
    exit 1
fi
if ! echo "$LEASE_ADDR" | grep -q "/"; then
    echo "FAIL: 600-e2e-show-json: dhcp.lease_address '$LEASE_ADDR' does not contain '/'" >&2
    exit 1
fi

# Verify dhcp.lease_time_secs = 120.
LEASE_TIME=$(echo "$DHCP_IFACE" | jq '.dhcp.lease_time_secs')
if [[ "$LEASE_TIME" != "120" ]]; then
    echo "FAIL: 600-e2e-show-json: dhcp.lease_time_secs is '$LEASE_TIME', expected 120" >&2
    exit 1
fi

# Verify dhcp.lease_remaining_secs is in [0, 120].
LEASE_REMAINING=$(echo "$DHCP_IFACE" | jq '.dhcp.lease_remaining_secs')
if [[ "$LEASE_REMAINING" == "null" ]] || [[ "$LEASE_REMAINING" -lt 0 ]] || [[ "$LEASE_REMAINING" -gt 120 ]]; then
    echo "FAIL: 600-e2e-show-json: dhcp.lease_remaining_secs is '$LEASE_REMAINING', expected integer in [0, 120]" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-json"
