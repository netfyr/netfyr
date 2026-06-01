#!/bin/bash
# 600-e2e-dhcp-reapply-no-ipv6-removal.sh
# End-to-end: After DHCP lease acquisition, re-applying the same policy
# must NOT produce changes in the journal. Specifically:
#
#   1. IPv6 link-local addresses must not appear as removed (-fe80::...)
#   2. IPv6 RA routes must not appear as removed (-::/0 via fe80::...)
#   3. The address must not appear as changed due to lifetime attributes
#   4. The default route must not appear as re-added
#
# Bug reproduction:
#   The diff engine compared DHCP-produced state (maps with lifetimes,
#   Value::String for routes) against kernel-queried state (IpNetwork
#   for addresses, IpAddr for gateways, plus IPv6 items). The type
#   mismatch and extra IPv6 items caused spurious changes.
#
# Topology:
#   Same nested-namespace DHCP topology as other DHCP e2e tests.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: dnsmasq not found" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: jq not found" >&2
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

# ── Set up nested network namespace for the DHCP server ──────────────────

ip link add veth-dhcp0 type veth peer name veth-dhcp1

unshare --net sleep 999 &
SERVER_NS_PID=$!
sleep 0.2

ip link set veth-dhcp1 netns "$SERVER_NS_PID"

nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set lo up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip link set veth-dhcp1 up
nsenter --net="/proc/$SERVER_NS_PID/ns/net" ip addr add 10.99.91.1/24 dev veth-dhcp1

ip link set veth-dhcp0 up

nsenter --net="/proc/$SERVER_NS_PID/ns/net" \
    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface=veth-dhcp1 \
        --dhcp-range="10.99.91.100,10.99.91.200,120" \
        --dhcp-option=3,10.99.91.1 \
        --dhcp-option=6,10.99.91.1 \
        --dhcp-leasefile="$TMPDIR_TEST/leases" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &
_DNSMASQ_PIDS+=($!)
sleep 1

# ── Start the daemon ─────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: daemon socket did not appear within 5s" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Step 1: Submit DHCP policy and wait for lease ────────────────────────

POLICY_FILE="$TMPDIR_TEST/dhcp-policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: dhcp-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"

wait_for_address veth-dhcp0 "10.99.91." 10

sleep 2

# ── Step 2: Re-apply the same policy ────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"

sleep 2

# ── Step 3: Find the re-apply journal entry ──────────────────────────────

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: journal file not found" >&2
    exit 1
fi

# The re-apply is the second policy_apply entry.
REAPPLY_ENTRY=$(jq -c 'select(.trigger.type == "policy_apply")' \
    "$JOURNAL_FILE" 2>/dev/null | tail -1 || true)

if [[ -z "$REAPPLY_ENTRY" ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: no policy_apply entry in journal" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# ── Step 4: Check for IPv6 address removal ───────────────────────────────
# The diff must NOT contain any field change referencing "fe80::" addresses.

IPV6_ADDR_CHANGES=$(echo "$REAPPLY_ENTRY" | jq '
    [.diff.operations[].field_changes[]
     | select(.field_name == "addresses")
     | select(
         (.current // {} | tostring | test("fe80::"))
         or (.desired // {} | tostring | test("fe80::"))
       )
    ] | length
')

if [[ "$IPV6_ADDR_CHANGES" -gt 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: re-apply diff references IPv6 addresses" >&2
    echo "      IPv6 link-local addresses must not appear as added/removed in the diff." >&2
    echo "      diff operations:" >&2
    echo "$REAPPLY_ENTRY" | jq '.diff.operations' >&2
    exit 1
fi

# ── Step 5: Check for IPv6 route removal ─────────────────────────────────

IPV6_ROUTE_CHANGES=$(echo "$REAPPLY_ENTRY" | jq '
    [.diff.operations[].field_changes[]
     | select(.field_name == "routes")
     | select(
         (.current // {} | tostring | test("::"))
         or (.desired // {} | tostring | test("::"))
       )
    ] | length
')

if [[ "$IPV6_ROUTE_CHANGES" -gt 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: re-apply diff references IPv6 routes" >&2
    echo "      IPv6 routes must not appear as added/removed in the diff." >&2
    echo "      diff operations:" >&2
    echo "$REAPPLY_ENTRY" | jq '.diff.operations' >&2
    exit 1
fi

# ── Step 6: Verify the re-apply diff is empty ────────────────────────────
# Re-applying the same DHCP policy with an existing lease should be a no-op.

REAPPLY_OPS_COUNT=$(echo "$REAPPLY_ENTRY" | jq '.diff.operations | length')

if [[ "$REAPPLY_OPS_COUNT" -gt 0 ]]; then
    echo "FAIL: 600-e2e-dhcp-reapply-no-ipv6-removal: re-apply diff has $REAPPLY_OPS_COUNT operations (expected 0)" >&2
    echo "      Re-applying a DHCP policy with an active lease should produce no changes." >&2
    echo "      diff operations:" >&2
    echo "$REAPPLY_ENTRY" | jq '.diff.operations' >&2
    exit 1
fi

echo "PASS: 600-e2e-dhcp-reapply-no-ipv6-removal"
