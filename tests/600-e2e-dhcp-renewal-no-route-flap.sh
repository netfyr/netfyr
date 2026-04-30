#!/bin/bash
# 600-e2e-dhcp-renewal-no-route-flap.sh
# End-to-end: After a DHCP lease is acquired, the queried state must not
# include kernel-managed prefix routes (proto kernel), and routes must not
# carry a "protocol" field that would cause a false diff on renewal.
#
# These two conditions are the root cause of:
#   1. Default route flip-flop on renewal (+dflt / -dflt) — the protocol
#      field in queried state doesn't match the desired state format.
#   2. Phantom kernel route removal (-3 routes) — kernel routes leak
#      into the queried state but are absent from desired state.
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-dhcp-renewal-no-route-flap.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: dnsmasq not found" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: jq not found" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# Create DHCP veth pair.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.80.1/24

# Start dnsmasq with gateway + DNS options so the lease includes a default
# route and dns_servers — exercising the full lease_to_state path.
dnsmasq \
    --no-daemon \
    --bind-dynamic \
    --interface=veth-dhcp1 \
    --dhcp-range=10.99.80.100,10.99.80.200,120 \
    --dhcp-option=3,10.99.80.1 \
    --dhcp-option=6,10.99.80.1 \
    --dhcp-leasefile="$TMPDIR_TEST/leases" \
    --no-resolv \
    --no-hosts \
    --log-dhcp \
    >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# Start the daemon with explicit journal directory.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket.
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: daemon socket did not appear within 5s" >&2
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
wait_for_address veth-dhcp0 "10.99.80." 10

# Give the daemon a moment to finish reconciliation and write the journal.
sleep 2

# ── Step 2: Extract the lease_acquired journal entry ────────────────────────
JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

ACQUIRED_ENTRY=$(jq -c 'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_acquired")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$ACQUIRED_ENTRY" ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: no lease_acquired entry in journal" >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# ── Step 3: Check state_after routes for kernel route leakage ───────────────
# The queried state (state_after) must not contain kernel routes.
KERNEL_ROUTES=$(echo "$ACQUIRED_ENTRY" | jq -c '
    [.state_after.entities[]
     | select(.selector_name == "veth-dhcp0")
     | .fields.routes[]
     | select(.protocol == "kernel")]
' 2>/dev/null || echo "[]")
KERNEL_COUNT=$(echo "$KERNEL_ROUTES" | jq 'length')

if [[ "$KERNEL_COUNT" -gt 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: kernel routes leaked into queried state" >&2
    echo "      kernel routes: $KERNEL_ROUTES" >&2
    exit 1
fi

# ── Step 4: Check that queried routes match desired routes format ───────────
# Routes in state_after must not carry a "protocol" field, since the desired
# state (from lease_to_state) does not include one. A mismatch would cause
# a false diff (route flap) on any subsequent reconciliation (renewal).
DESIRED_ROUTES=$(echo "$ACQUIRED_ENTRY" | jq -c '
    .diff.operations[]
    | select(.entity_name == "veth-dhcp0")
    | .field_changes[]
    | select(.field_name == "routes")
    | .desired // []
' 2>/dev/null || echo "[]")

STATE_ROUTES=$(echo "$ACQUIRED_ENTRY" | jq -c '
    [.state_after.entities[]
     | select(.selector_name == "veth-dhcp0")
     | .fields.routes[]]
' 2>/dev/null || echo "[]")

PROTOCOL_ROUTES=$(echo "$STATE_ROUTES" | jq -c '[.[] | select(has("protocol"))]')
PROTOCOL_COUNT=$(echo "$PROTOCOL_ROUTES" | jq 'length')

if [[ "$PROTOCOL_COUNT" -gt 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: routes in queried state have 'protocol' field" >&2
    echo "      This would cause a route flap on renewal (desired vs actual mismatch)." >&2
    echo "      routes with protocol: $PROTOCOL_ROUTES" >&2
    echo "      desired routes: $DESIRED_ROUTES" >&2
    exit 1
fi

# ── Step 5: Verify desired default route is present in state_after ──────────
# The default route from the DHCP lease must appear in both desired and
# state_after, with identical fields (no extra keys).
DESIRED_DEFAULT=$(echo "$DESIRED_ROUTES" | jq -c '
    if type == "array" then [.[] | select(.destination == "0.0.0.0/0")] | .[0] // empty
    else empty end
' 2>/dev/null || true)
STATE_DEFAULT=$(echo "$STATE_ROUTES" | jq -c '
    [.[] | select(.destination == "0.0.0.0/0")] | .[0] // empty
' 2>/dev/null || true)

if [[ -z "$STATE_DEFAULT" ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: no default route in queried state" >&2
    echo "      state routes: $STATE_ROUTES" >&2
    exit 1
fi

if [[ -n "$DESIRED_DEFAULT" && "$DESIRED_DEFAULT" != "$STATE_DEFAULT" ]]; then
    echo "FAIL: 600-e2e-dhcp-renewal-no-route-flap: default route mismatch" >&2
    echo "      desired: $DESIRED_DEFAULT" >&2
    echo "      state:   $STATE_DEFAULT" >&2
    echo "      A mismatch here causes +dflt/-dflt route flap on renewal." >&2
    exit 1
fi

echo "PASS: 600-e2e-dhcp-renewal-no-route-flap"
