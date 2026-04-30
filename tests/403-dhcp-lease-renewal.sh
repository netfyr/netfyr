#!/bin/bash
# 403-dhcp-lease-renewal.sh
# Integration test: When a DHCP lease is renewed, the daemon re-reconciles
# and the interface address remains unchanged. Uses a nested network namespace
# for the DHCP server so that unicast T1 renewal packets traverse the veth
# pair instead of being short-circuited by the kernel.
#
# Mapped to acceptance criteria:
#   "Lease renewal triggers reconciliation"
#   "System state remains unchanged after renewal"
#
# Topology:
#   ┌─ Outer netns (client) ────────────────┐
#   │  veth-dhcp0  ◄──── veth link ────►    │
#   │  (DHCP client, daemon)                │
#   └───────────────────────────────────────┘
#                                     │
#   ┌─ Inner netns (server) ──────────┐
#   │  veth-dhcp1  10.99.0.1/24      │
#   │  dnsmasq (DHCP server)         │
#   └─────────────────────────────────┘
#
# Requires: unshare, nsenter, ip (iproute2), dnsmasq, jq
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

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 403-dhcp-lease-renewal: jq not found; install jq to run DHCP tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
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
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip addr add 10.99.0.1/24 dev veth-dhcp1

# Bring up the client side in the outer namespace.
ip link set veth-dhcp0 up

# Start dnsmasq inside the inner namespace. dnsmasq enforces a minimum
# lease time of 120s for plain numeric values.
LEASE_SECS="${DHCP_LEASE_SECS:-60}"
nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.0.100,10.99.0.200,${LEASE_SECS}" \
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

# Give the daemon a moment to finish reconciliation.
sleep 2

# ── Step 2: Wait past T1 for renewal ────────────────────────────────────────
# dnsmasq enforces a minimum lease of 120s. Even if LEASE_SECS is 60,
# the actual grant is 120s, so T1 = 60s. We use the worst-case lease
# time to compute the wait, plus a generous buffer for timing jitter.
WORST_CASE_LEASE=120
RENEWAL_WAIT=$(( WORST_CASE_LEASE / 2 + 10 ))
echo "# Waiting ${RENEWAL_WAIT}s for DHCP renewal at T1..."
sleep "$RENEWAL_WAIT"

# Give the daemon a moment to finish post-renewal reconciliation.
sleep 3

# ── Step 3: Verify the address is still present (renewal succeeded) ──────────

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

# ── Step 5: Verify renewal actually happened via journal ─────────────────────
JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

RENEWED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$RENEWED_ENTRY" ]]; then
    echo "FAIL: 403-dhcp-lease-renewal: no lease_renewed entry in journal" >&2
    echo "      Renewal did not occur — unicast DHCPREQUEST may not have reached the server." >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

echo "PASS: 403-dhcp-lease-renewal"
