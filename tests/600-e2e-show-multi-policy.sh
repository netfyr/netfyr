#!/bin/bash
# 600-e2e-show-multi-policy.sh -- End-to-end: netfyr show with multiple policies on one interface.
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-multi-policy: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-multi-policy: jq not found; install jq to run JSON validation tests" >&2
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

# Create veth pair: veth-multi1 is the server side for dnsmasq.
create_veth veth-multi0 veth-multi1
add_address veth-multi1 10.99.1.1/24

# Start dnsmasq with 120s lease on the server side.
start_dnsmasq veth-multi1 10.99.1.1 10.99.1.100 10.99.1.200 120

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-multi-policy: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-multi-policy: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write two policies both targeting veth-multi0: a static MTU policy and a DHCP policy.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/mtu.yaml" <<'EOF'
kind: policy
name: e2e-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-multi0
  mtu: 1400
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-dhcp
factory: dhcpv4
selector:
  name: veth-multi0
EOF

# Apply both policies from the directory.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP address (up to 10 seconds).
wait_for_address veth-multi0 "10.99.1." 10

# --- Text output assertions ---

SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Verify both policy names appear in the output.
if ! echo "$SHOW_OUTPUT" | grep -q "e2e-mtu"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain 'e2e-mtu'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "e2e-dhcp"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain 'e2e-dhcp'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify both policy types appear.
if ! echo "$SHOW_OUTPUT" | grep -q "(static)"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain '(static)'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(dhcpv4)"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain '(dhcpv4)'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify DHCP: running.
if ! echo "$SHOW_OUTPUT" | grep -q "DHCP:.*running"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain 'DHCP:.*running'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Lease: 120s total.
if ! echo "$SHOW_OUTPUT" | grep -q "120s total"; then
    echo "FAIL: 600-e2e-show-multi-policy: show output does not contain '120s total'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# --- JSON output assertions ---

JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)

if ! echo "$JSON_OUTPUT" | jq -e 'type == "object"' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-multi-policy: show -o json output is not a JSON object" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

# Find veth-multi0 entry.
MULTI_IFACE=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-multi0")')
if [[ -z "$MULTI_IFACE" ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: veth-multi0 not found in interfaces array" >&2
    echo "      JSON: $JSON_OUTPUT" >&2
    exit 1
fi

# Verify policies array has exactly 2 elements.
POLICY_COUNT=$(echo "$MULTI_IFACE" | jq '.policies | length')
if [[ "$POLICY_COUNT" -ne 2 ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: veth-multi0 has $POLICY_COUNT policies, expected 2" >&2
    echo "      iface: $MULTI_IFACE" >&2
    exit 1
fi

# Verify one policy has type "static" and the other has type "dhcpv4".
HAS_STATIC=$(echo "$MULTI_IFACE" | jq '[.policies[] | select(.type == "static")] | length')
HAS_DHCPV4=$(echo "$MULTI_IFACE" | jq '[.policies[] | select(.type == "dhcpv4")] | length')
if [[ "$HAS_STATIC" -ne 1 ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: expected 1 static policy, got $HAS_STATIC" >&2
    exit 1
fi
if [[ "$HAS_DHCPV4" -ne 1 ]]; then
    echo "FAIL: 600-e2e-show-multi-policy: expected 1 dhcpv4 policy, got $HAS_DHCPV4" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-multi-policy"
