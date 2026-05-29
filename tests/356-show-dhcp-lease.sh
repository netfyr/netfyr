#!/bin/bash
# 356-show-dhcp-lease.sh -- End-to-end: netfyr show displays DHCP factory with lease timing.
#
# Requires: unshare, ip (iproute2), dnsmasq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 356-show-dhcp-lease: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 356-show-dhcp-lease: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 356-show-dhcp-lease: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
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
        echo "FAIL: 356-show-dhcp-lease: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 356-show-dhcp-lease: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a DHCP policy for veth-dhcp0.
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-show-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 356-show-dhcp-lease: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease to be acquired (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# Run netfyr show and capture output.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Verify Policies: line contains the policy name and type.
if ! echo "$SHOW_OUTPUT" | grep -q "e2e-show-dhcp"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain policy name 'e2e-show-dhcp'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(dhcpv4)"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain '(dhcpv4)'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify DHCP: running.
if ! echo "$SHOW_OUTPUT" | grep -q "DHCP:.*running"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain 'DHCP:.*running'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Lease: line with 120s total.
if ! echo "$SHOW_OUTPUT" | grep -q "Lease:"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain 'Lease:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "120s total"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain '120s total'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "remaining"; then
    echo "FAIL: 356-show-dhcp-lease: show output does not contain 'remaining'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 356-show-dhcp-lease"
