#!/bin/bash
# 356-show-mixed.sh -- End-to-end: netfyr show displays both static and DHCP managed interfaces.
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 356-show-mixed: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 356-show-mixed: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 356-show-mixed: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 356-show-mixed: jq not found; install jq to run JSON validation tests" >&2
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

# Static interface pair.
create_veth veth-static0 veth-static1

# DHCP interface pair: veth-dhcp1 is the server side.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24

# Start dnsmasq DHCP server.
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
        echo "FAIL: 356-show-mixed: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 356-show-mixed: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write static and DHCP policies.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/static.yaml" <<'EOF'
kind: policy
name: e2e-mixed-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-static0
  mtu: 1400
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-mixed-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply both policies.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 356-show-mixed: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease to be acquired (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# --- Text output assertions ---

SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Static interface has Policies: with (static).
if ! echo "$SHOW_OUTPUT" | grep -q "veth-static0"; then
    echo "FAIL: 356-show-mixed: show output does not contain 'veth-static0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(static)"; then
    echo "FAIL: 356-show-mixed: show output does not contain '(static)' policy type" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# DHCP interface has Policies: with (dhcpv4) and DHCP: running.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-dhcp0"; then
    echo "FAIL: 356-show-mixed: show output does not contain 'veth-dhcp0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(dhcpv4)"; then
    echo "FAIL: 356-show-mixed: show output does not contain '(dhcpv4)' policy type" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "DHCP:.*running"; then
    echo "FAIL: 356-show-mixed: show output does not contain 'DHCP:.*running'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "120s total"; then
    echo "FAIL: 356-show-mixed: show output does not contain '120s total'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Unmanaged peer (veth-static1) should appear without Policies: line.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-static1"; then
    echo "FAIL: 356-show-mixed: show output does not contain unmanaged 'veth-static1'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# --- JSON output assertions ---

JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)

if ! echo "$JSON_OUTPUT" | jq -e 'type == "object"' >/dev/null 2>&1; then
    echo "FAIL: 356-show-mixed: show -o json output is not a JSON object" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

DAEMON_STATUS=$(echo "$JSON_OUTPUT" | jq -r '.daemon.status')
if [[ "$DAEMON_STATUS" != "running" ]]; then
    echo "FAIL: 356-show-mixed: daemon.status is '$DAEMON_STATUS', expected 'running'" >&2
    exit 1
fi

IFACE_COUNT=$(echo "$JSON_OUTPUT" | jq '.interfaces | length')
if [[ "$IFACE_COUNT" -lt 2 ]]; then
    echo "FAIL: 356-show-mixed: interfaces array has $IFACE_COUNT elements, expected at least 2" >&2
    exit 1
fi

# Verify veth-dhcp0 has dhcp field with state running.
DHCP_STATE=$(echo "$JSON_OUTPUT" | jq -r '.interfaces[] | select(.name == "veth-dhcp0") | .dhcp.state')
if [[ "$DHCP_STATE" != "running" ]]; then
    echo "FAIL: 356-show-mixed: veth-dhcp0 dhcp.state is '$DHCP_STATE', expected 'running'" >&2
    exit 1
fi

echo "PASS: 356-show-mixed"
