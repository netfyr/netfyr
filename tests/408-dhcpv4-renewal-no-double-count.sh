#!/bin/bash
# 408-dhcpv4-renewal-no-double-count.sh
# End-to-end: DHCP lease renewal with no effective change must produce exactly
# one new journal entry (not duplicate entries for the same renewal event).
#
# Topology: same nested-namespace setup as 408-dhcpv4-renewal-no-spurious-diff.
# Uses subnet 10.99.92.x to avoid collisions when tests run in parallel.
#
# Requires: unshare, nsenter, ip (iproute2), dnsmasq, jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: dnsmasq not found" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: jq not found" >&2
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
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip addr add 10.99.92.1/24 dev veth-dhcp1

ip link set veth-dhcp0 up

LEASE_SECS="${DHCP_LEASE_SECS:-60}"
nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.92.100,10.99.92.200,${LEASE_SECS}" \
        --dhcp-option=3,10.99.92.1 \
        --dhcp-option=6,10.99.92.1 \
        --dhcp-leasefile="$TMPDIR_TEST/leases" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# ── Start the daemon ────────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 408-dhcpv4-renewal-no-double-count: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 408-dhcpv4-renewal-no-double-count: daemon socket did not appear within 5s" >&2
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
wait_for_address veth-dhcp0 "10.99.92." 15

# Give the daemon time to finish reconciliation and write the journal.
sleep 2

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: journal file not found at $JOURNAL_FILE" >&2
    exit 1
fi

# Count lease_renewed entries before the renewal window.
BEFORE_COUNT=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | wc -l || echo 0)

# ── Step 2: Wait past T1 for renewal ───────────────────────────────────────
WORST_CASE_LEASE=120
RENEWAL_WAIT=$(( WORST_CASE_LEASE / 2 + 10 ))
echo "# Waiting ${RENEWAL_WAIT}s for DHCP renewal at T1..."
sleep "$RENEWAL_WAIT"

# Give the daemon time to finish post-renewal processing and journal write.
sleep 3

# ── Step 3: Verify daemon is still running ──────────────────────────────────
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: daemon exited during renewal" >&2
    exit 1
fi

# ── Step 4: Count new lease_renewed entries ─────────────────────────────────
AFTER_COUNT=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | wc -l || echo 0)

NEW_ENTRIES=$(( AFTER_COUNT - BEFORE_COUNT ))

if [[ "$NEW_ENTRIES" -lt 1 ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: no new lease_renewed entry found after renewal wait" >&2
    echo "      This means DHCP renewal did not occur within the wait window." >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

if [[ "$NEW_ENTRIES" -gt 1 ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: $NEW_ENTRIES new lease_renewed entries (expected 1)" >&2
    echo "      The daemon produced duplicate journal entries for a single T1 renewal event." >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# ── Step 5: Verify the renewal entry has zero diff operations ───────────────
RENEWED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

RENEWAL_OPS_COUNT=$(echo "$RENEWED_ENTRY" | jq '.diff.operations | length')

if [[ "$RENEWAL_OPS_COUNT" -gt 0 ]]; then
    echo "FAIL: 408-dhcpv4-renewal-no-double-count: renewal diff has $RENEWAL_OPS_COUNT operations (expected 0)" >&2
    echo "      A renewal with the same IP must produce an empty diff." >&2
    echo "      renewal diff operations:" >&2
    echo "$RENEWED_ENTRY" | jq '.diff.operations' >&2
    exit 1
fi

echo "PASS: 408-dhcpv4-renewal-no-double-count"
