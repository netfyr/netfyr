#!/bin/bash
# 408-dhcpv4-renewal-no-spurious-diff.sh
# End-to-end: After DHCP lease renewal with unchanged lease parameters,
# the journal entry for the renewal must NOT contain spurious field changes.
#
# Specifically validates the x-netfyr-comparison-keys behavior: the "addresses"
# field declares comparison-keys=["address"], so a renewal that keeps the same
# IP but changes valid_lft must produce zero diff operations (not a spurious
# Modify on the addresses field).
#
# Topology:
#   The DHCP server (dnsmasq) runs in a nested network namespace so that
#   unicast T1 renewal packets traverse the veth pair instead of being
#   short-circuited by the kernel (which happens when both ends are in
#   the same netns).
#
#     ┌─ Outer netns (client) ─────────────────┐
#     │  veth-dhcp0  ◄──── veth link ────►      │
#     │  (DHCP client, daemon)                  │
#     └─────────────────────────────────────────┘
#                                         │
#     ┌─ Inner netns (server) ─────────────┐
#     │  veth-dhcp1  10.99.91.1/24        │
#     │  dnsmasq (DHCP server)            │
#     └────────────────────────────────────┘
#
# Requires: unshare, nsenter, ip (iproute2), dnsmasq, jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: dnsmasq not found" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: jq not found" >&2
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

# Create the veth pair in the outer namespace.
ip link add veth-dhcp0 type veth peer name veth-dhcp1

# Spawn a long-lived process in a new network namespace to hold it open.
unshare --net sleep 999 &
SERVER_NS_PID=$!
sleep 0.2

# Move the server end of the veth pair into the inner namespace.
ip link set veth-dhcp1 netns "$SERVER_NS_PID"

# Configure the server side inside the inner namespace.
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set lo up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set veth-dhcp1 up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip addr add 10.99.91.1/24 dev veth-dhcp1

# Bring up the client side in the outer namespace.
ip link set veth-dhcp0 up

# Start dnsmasq inside the inner namespace with gateway + DNS options.
# dnsmasq enforces a minimum lease time of 120s.
LEASE_SECS="${DHCP_LEASE_SECS:-60}"
nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.91.100,10.99.91.200,${LEASE_SECS}" \
        --dhcp-option=3,10.99.91.1 \
        --dhcp-option=6,10.99.91.1 \
        --dhcp-leasefile="$TMPDIR_TEST/leases" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# ── Start the daemon in the outer namespace ────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket.
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: daemon socket did not appear within 5s" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit a DHCPv4 policy.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dhcp-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"

# ── Step 1: Wait for initial lease ──────────────────────────────────────────
wait_for_address veth-dhcp0 "10.99.91." 15

# Give the daemon a moment to finish reconciliation and write the journal.
sleep 2

# ── Step 2: Wait past T1 for renewal ───────────────────────────────────────
# dnsmasq enforces a minimum lease of 120s. Even if LEASE_SECS is 60,
# the actual grant is 120s, so T1 = 60s. We wait well past T1.
WORST_CASE_LEASE=120
RENEWAL_WAIT=$(( WORST_CASE_LEASE / 2 + 10 ))
echo "# Waiting ${RENEWAL_WAIT}s for DHCP renewal at T1..."
sleep "$RENEWAL_WAIT"

# Give the daemon a moment to finish post-renewal reconciliation and journal write.
sleep 3

# ── Step 3: Verify daemon is still running ──────────────────────────────────
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: daemon exited during renewal" >&2
    exit 1
fi

# ── Step 4: Find the lease_renewed journal entry ────────────────────────────
JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: journal file not found at $JOURNAL_FILE" >&2
    exit 1
fi

RENEWED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$RENEWED_ENTRY" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: no lease_renewed entry in journal" >&2
    echo "      This means DHCP renewal did not occur within the wait window." >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# ── Step 5: Verify renewal diff is empty (comparison-key assertion) ─────────
# When valid_lft changes during renewal but the address key is identical,
# x-netfyr-comparison-keys=["address"] must suppress the spurious Modify.
# If comparison keys were missing from ip.json, this count would be > 0.
RENEWAL_OPS_COUNT=$(echo "$RENEWED_ENTRY" | jq '.diff.operations | length')

if [[ "$RENEWAL_OPS_COUNT" -gt 0 ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-spurious-diff: renewal diff has $RENEWAL_OPS_COUNT operations (expected 0)" >&2
    echo "      A renewal with the same IP must produce an empty diff even when valid_lft changes." >&2
    echo "      If x-netfyr-comparison-keys is missing from the addresses field in ip.json," >&2
    echo "      the diff engine treats the lifetime change as an address add/remove." >&2
    echo "      renewal diff operations:" >&2
    echo "$RENEWED_ENTRY" | jq '.diff.operations' >&2
    exit 1
fi

# ── Step 6: Verify the address remains on the interface ────────────────────
assert_has_address veth-dhcp0 "10.99.91."

echo "PASS: 408-dhcpv4-renewal-no-spurious-diff"
