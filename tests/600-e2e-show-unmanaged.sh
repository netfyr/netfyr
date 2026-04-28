#!/bin/bash
# 600-e2e-show-unmanaged.sh -- End-to-end: netfyr show lists all interfaces including unmanaged ones.
#
# Requires: unshare, ip (iproute2), dnsmasq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-unmanaged: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-unmanaged: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-unmanaged: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
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

# Three veth pairs: one managed (static), one fully unmanaged, one DHCP.
create_veth veth-managed0 veth-managed1
create_veth veth-other0 veth-other1
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
        echo "FAIL: 600-e2e-show-unmanaged: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-unmanaged: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write policies only for veth-managed0 (static) and veth-dhcp0 (DHCP).
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/managed.yaml" <<'EOF'
kind: policy
name: e2e-unmanaged-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-managed0
  mtu: 1400
EOF

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-unmanaged-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply both policies.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-unmanaged: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# Run netfyr show.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Verify managed interface with static policy appears.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-managed0"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain 'veth-managed0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(static)"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain '(static)' for veth-managed0" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify DHCP managed interface appears with DHCP state.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-dhcp0"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain 'veth-dhcp0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "(dhcpv4)"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain '(dhcpv4)'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify unmanaged interfaces appear (bare names).
if ! echo "$SHOW_OUTPUT" | grep -q "veth-other0"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain unmanaged 'veth-other0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "veth-managed1"; then
    echo "FAIL: 600-e2e-show-unmanaged: show output does not contain unmanaged peer 'veth-managed1'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-unmanaged"
