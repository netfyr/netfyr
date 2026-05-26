#!/bin/bash
# 401-dhcpv4-renewal-no-route-flap.sh
# Integration test: DHCP lease renewal does not cause a spurious add+remove
# cycle on the default route (route flap).
# Mapped to acceptance criteria: "DHCP renewal does not flap routes".
#
# The DHCP server runs in a nested network namespace so that unicast T1 renewal
# packets traverse the veth pair instead of being short-circuited by the kernel
# (which happens when both ends are in the same netns).
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
#   bash tests/401-dhcpv4-renewal-no-route-flap.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: jq not found; install jq to run DHCP tests" >&2
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

# ── Set up nested network namespace for the DHCP server ─────────────────────

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

# Start dnsmasq inside the inner namespace with gateway option so the lease
# includes a default route — required to test for route flap. dnsmasq enforces
# a minimum lease time of 120s for plain numeric values.
LEASE_SECS="${DHCP_LEASE_SECS:-60}"
nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.0.100,10.99.0.200,${LEASE_SECS}" \
        --dhcp-option=3,10.99.0.1 \
        --dhcp-option=6,10.99.0.1 \
        --dhcp-leasefile="$TMPDIR_TEST/leases" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# ── Start the daemon ──────────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 401-dhcpv4-renewal-no-route-flap: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 401-dhcpv4-renewal-no-route-flap: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit a DHCPv4 policy for veth-dhcp0.
POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dhcp-route-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"
APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Step 1: Wait for the initial lease ───────────────────────────────────────

wait_for_address veth-dhcp0 "10.99.0." 10

# Verify the default route is present after initial lease acquisition.
if ! ip route show default 2>/dev/null | grep -q "10.99.0.1"; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: default route via 10.99.0.1 not present after initial lease" >&2
    echo "      ip route show default:" >&2
    ip route show default >&2 || true
    exit 1
fi

# Give the daemon a moment to finish reconciliation.
sleep 2

# ── Step 2: Wait past T1 for renewal ─────────────────────────────────────────
# dnsmasq enforces a minimum lease of 120s. Even if LEASE_SECS is 60, the
# actual grant is 120s, so T1 = 60s. We use the worst-case lease time to
# compute the wait, plus a generous buffer for timing jitter.
WORST_CASE_LEASE=120
RENEWAL_WAIT=$(( WORST_CASE_LEASE / 2 + 10 ))
echo "# Waiting ${RENEWAL_WAIT}s for DHCP renewal at T1..."
sleep "$RENEWAL_WAIT"

# Give the daemon a moment to finish post-renewal reconciliation.
sleep 3

# ── Step 3: Daemon must still be running ─────────────────────────────────────

if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: daemon exited during lease renewal period" >&2
    exit 1
fi

# ── Step 4: Verify address and route are still present ───────────────────────

assert_has_address veth-dhcp0 "10.99.0."

if ! ip route show default 2>/dev/null | grep -q "10.99.0.1"; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: default route via 10.99.0.1 disappeared after renewal" >&2
    echo "      The route was present before renewal but is gone now — this is a route flap." >&2
    echo "      ip route show default:" >&2
    ip route show default >&2 || true
    exit 1
fi

# ── Step 5: Verify renewal actually happened via journal ─────────────────────

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: journal file not found at $JOURNAL_FILE" >&2
    exit 1
fi

RENEWED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_renewed")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$RENEWED_ENTRY" ]]; then
    echo "FAIL: 401-dhcpv4-renewal-no-route-flap: no lease_renewed entry in journal" >&2
    echo "      Renewal did not occur — unicast DHCPREQUEST may not have reached the server." >&2
    echo "      Without a confirmed renewal, the route stability test is a false positive." >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

echo "PASS: 401-dhcpv4-renewal-no-route-flap"
