#!/bin/bash
# 356-show-dhcp-waiting.sh -- End-to-end: netfyr show displays waiting DHCP factory (no server).
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 356-show-dhcp-waiting: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 356-show-dhcp-waiting: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

# Create veth pair — no DHCP server, so the factory will be in waiting state.
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
        echo "FAIL: 356-show-dhcp-waiting: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 356-show-dhcp-waiting: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a DHCP policy for veth-dhcp0 (no server listening — factory enters waiting state).
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-show-wait
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 356-show-dhcp-waiting: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Poll for the DHCP factory to enter waiting state (up to 30 seconds).
WAIT_ITERS=0
SHOW_OUTPUT=""
while true; do
    SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)
    if echo "$SHOW_OUTPUT" | grep -q "DHCP:.*waiting"; then
        break
    fi
    if (( WAIT_ITERS >= 300 )); then
        echo "FAIL: 356-show-dhcp-waiting: DHCP factory did not enter 'waiting' state within 30 seconds" >&2
        echo "      output: $SHOW_OUTPUT" >&2
        exit 1
    fi
    sleep 0.1
    (( WAIT_ITERS++ )) || true
done

# Verify the policy name appears.
if ! echo "$SHOW_OUTPUT" | grep -q "e2e-show-wait"; then
    echo "FAIL: 356-show-dhcp-waiting: show output does not contain policy name 'e2e-show-wait'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify DHCP: waiting.
if ! echo "$SHOW_OUTPUT" | grep -q "DHCP:.*waiting"; then
    echo "FAIL: 356-show-dhcp-waiting: show output does not contain 'DHCP:.*waiting'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify no Lease: line appears (waiting factory has no lease).
if echo "$SHOW_OUTPUT" | grep -q "Lease:"; then
    echo "FAIL: 356-show-dhcp-waiting: show output unexpectedly contains 'Lease:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 356-show-dhcp-waiting"
