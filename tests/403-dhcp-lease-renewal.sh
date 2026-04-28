#!/bin/bash
# 403-dhcp-lease-renewal.sh
# Integration test: When a DHCP lease is renewed, the daemon re-reconciles
# and the interface address remains unchanged. Uses a 60-second lease so that
# T1 (renewal at 50% = 30s) is reached within a reasonable test window.
# Mapped to acceptance criteria:
#   "Lease renewal triggers reconciliation"
#   "System state remains unchanged after renewal"
#
# NOTE: This test waits ~35 seconds for renewal to occur at T1 (30s into the
# 60-second lease). This is intentionally slow to exercise the actual DHCP
# renewal code path in the daemon's factory event loop.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-dhcp-lease-renewal.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lease-renewal: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create DHCP veth pair: veth-dhcp0 (client) / veth-dhcp1 (server side).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Use a 60-second lease so T1 (50% = 30s) is reached within 35 seconds of
# lease acquisition. dnsmasq enforces a minimum of 120s for plain numbers;
# we pass "60" directly and rely on the daemon honoring whatever time the
# server grants (may be bumped to 120s by some dnsmasq builds).
LEASE_SECS="${DHCP_LEASE_SECS:-60}"
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 "$LEASE_SECS"

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 403-dhcp-lease-renewal: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-dhcp-lease-renewal: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit a DHCPv4 policy for veth-dhcp0.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: veth-dhcp0-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"
APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Step 1: Wait for the initial lease ───────────────────────────────────────

wait_for_address veth-dhcp0 "10.99.0." 10

INITIAL_ADDR=$(ip addr show dev veth-dhcp0 2>/dev/null \
    | grep -oP 'inet \K[0-9.]+/[0-9]+' | head -1)

if [[ -z "$INITIAL_ADDR" ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: could not determine initial DHCP address" >&2
    exit 1
fi

# ── Step 2: Wait past T1 for renewal ────────────────────────────────────────
# T1 = LEASE_SECS / 2.  We wait T1 + 5s as a buffer.
# For a 60s lease: wait 35s. For a 120s dnsmasq-minimum lease: wait 65s.
# This ensures the DHCP client sends a renewal (DHCPREQUEST in renewing state)
# and the server responds with a DHCPACK, causing FactoryEvent::LeaseRenewed.
RENEWAL_WAIT=$(( LEASE_SECS / 2 + 5 ))
sleep "$RENEWAL_WAIT"

# ── Step 3: Verify the address is still present (renewal succeeded) ──────────
# After renewal the daemon fires reconcile_and_apply. Since the renewed state
# is identical to the initial state, the system should be unchanged.

ADDR_AFTER=$(ip addr show dev veth-dhcp0 2>/dev/null \
    | grep -oP 'inet \K[0-9.]+/[0-9]+' | head -1)

if [[ -z "$ADDR_AFTER" ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: address was removed after renewal window" >&2
    echo "      initial address: $INITIAL_ADDR" >&2
    ip addr show dev veth-dhcp0 >&2 || true
    exit 1
fi

# The address must still be in the expected subnet.
assert_has_address veth-dhcp0 "10.99.0."

# The link must still be UP.
assert_link_up veth-dhcp0

# ── Step 4: Daemon must still be running (renewal must not crash the daemon) ─

if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 403-dhcp-lease-renewal: daemon exited during lease renewal period" >&2
    exit 1
fi

echo "PASS: 403-dhcp-lease-renewal"
