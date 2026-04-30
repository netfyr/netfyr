#!/bin/bash
# 403-dhcp-lifetime-after-renewal.sh
# Integration test: After a DHCP lease renewal, the kernel address lifetime
# must be refreshed back to near the full lease time, not left decaying toward
# zero.
#
# Uses a nested network namespace so unicast T1 renewal packets traverse the
# veth pair (same topology as 403-dhcp-lease-renewal.sh).
#
# Requires: unshare, nsenter, ip (iproute2), dnsmasq, jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-dhcp-lifetime-after-renewal.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: dnsmasq not found" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: jq not found" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the outer user+network namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
SERVER_NS_PID=""

cleanup_all() {
    kill "${DAEMON_PID:-}" 2>/dev/null || true
    local pid
    for pid in "${_DNSMASQ_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    _DNSMASQ_PIDS=()
    kill "${SERVER_NS_PID:-}" 2>/dev/null || true
    rm -rf "$TMPDIR_TEST"
}
trap cleanup_all EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# ── Set up nested network namespace for the DHCP server ────────────────────

ip link add veth-dhcp0 type veth peer name veth-dhcp1

unshare --net sleep 999 &
SERVER_NS_PID=$!
sleep 0.2

ip link set veth-dhcp1 netns "$SERVER_NS_PID"

nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set lo up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set veth-dhcp1 up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip addr add 10.99.0.1/24 dev veth-dhcp1

ip link set veth-dhcp0 up

# dnsmasq enforces a minimum lease of 120s.
nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.0.100,10.99.0.200,120" \
        --dhcp-leasefile="$TMPDIR_TEST/leases" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# ── Start the daemon ──────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 403-dhcp-lifetime-after-renewal: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-dhcp-lifetime-after-renewal: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply DHCP policy ────────────────────────────────────────────────────

POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dhcp-renewal-lft-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"

# ── Step 1: Wait for the initial lease and verify finite lifetime ──────────

wait_for_address veth-dhcp0 "10.99.0." 10
sleep 1

assert_valid_lft_finite veth-dhcp0 "10.99.0."

INITIAL_LFT=$(get_valid_lft_secs veth-dhcp0 "10.99.0.")
echo "# Initial valid_lft: ${INITIAL_LFT}sec"

# ── Step 2: Wait past T1 for renewal ──────────────────────────────────────
# Lease = 120s (dnsmasq minimum), T1 = 60s.  Wait T1 + buffer.
RENEWAL_WAIT=70
echo "# Waiting ${RENEWAL_WAIT}s for DHCP renewal at T1..."
sleep "$RENEWAL_WAIT"

# Give the daemon time to re-reconcile after renewal.
sleep 3

# ── Step 3: Verify renewal happened ──────────────────────────────────────

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"
RENEWED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$RENEWED_ENTRY" ]]; then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: no lease_renewed entry in journal" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# ── Step 4: Verify lifetime was refreshed ─────────────────────────────────
# After ~73s (70s wait + 3s buffer), a non-refreshed lifetime would have
# decayed from 120s to ~47s.  A refreshed lifetime should be back near 120s.
# We assert > 60s (half of lease time) as a safe threshold.

RENEWED_LFT=$(get_valid_lft_secs veth-dhcp0 "10.99.0.")
echo "# Post-renewal valid_lft: ${RENEWED_LFT}sec"

if (( RENEWED_LFT <= 60 )); then
    echo "FAIL: 403-dhcp-lifetime-after-renewal: valid_lft after renewal is ${RENEWED_LFT}sec, expected > 60sec" >&2
    echo "      Lifetime was not refreshed by the daemon on renewal." >&2
    ip addr show dev veth-dhcp0 >&2 || true
    exit 1
fi

echo "PASS: 403-dhcp-lifetime-after-renewal"
