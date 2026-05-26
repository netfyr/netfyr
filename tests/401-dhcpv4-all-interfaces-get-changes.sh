#!/bin/bash
# 401-dhcpv4-all-interfaces-get-changes.sh
# Integration test: when multiple DHCP interfaces acquire leases, EVERY
# lease_acquired journal entry must contain address field changes — not just
# the first one processed.
# Mapped to acceptance criteria: "Reconciliation applies changes to all managed
# interfaces, not just the DHCP one".
#
# Topology:
#   veth-dhcp0 <──veth──> veth-dhcp1  (dnsmasq 10.99.1.0/24)
#   veth-dhcp2 <──veth──> veth-dhcp3  (dnsmasq 10.99.2.0/24)
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/401-dhcpv4-all-interfaces-get-changes.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: jq not found; install jq to run DHCP tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# ── Create two DHCP interface pairs on separate subnets ──────────────────────

create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

create_veth veth-dhcp2 veth-dhcp3
add_address veth-dhcp3 10.99.2.1/24
start_dnsmasq veth-dhcp3 10.99.2.1 10.99.2.100 10.99.2.200 120

# ── Start the daemon ──────────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Submit two DHCPv4 policies ───────────────────────────────────────────────

APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/dhcp0.yaml" <<'EOF'
kind: policy
name: dhcp-iface0
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

cat > "$APPLY_DIR/dhcp2.yaml" <<'EOF'
kind: policy
name: dhcp-iface2
factory: dhcpv4
selector:
  name: veth-dhcp2
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Wait for both leases ─────────────────────────────────────────────────────

wait_for_address veth-dhcp0 "10.99.1." 10
wait_for_address veth-dhcp2 "10.99.2." 10

# Give the daemon time to finish reconciliation and journal writes.
sleep 2

# ── Verify every lease_acquired entry has address changes ────────────────────

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: journal file not found at $JOURNAL_FILE" >&2
    exit 1
fi

# Extract all lease_acquired entries.
ACQUIRED_ENTRIES=$(jq -c \
    'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_acquired")' \
    "$JOURNAL_FILE" 2>/dev/null || true)

ACQUIRED_COUNT=$(echo "$ACQUIRED_ENTRIES" | grep -c '^{' || true)

if [[ "$ACQUIRED_COUNT" -lt 2 ]]; then
    echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: expected at least 2 lease_acquired entries, got $ACQUIRED_COUNT" >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# Each lease_acquired entry must have at least one field change. An empty diff
# means the address was applied as a side effect of another interface's event.
ENTRY_IDX=0
while IFS= read -r entry; do
    ENTRY_IDX=$((ENTRY_IDX + 1))

    POLICY_NAME=$(echo "$entry" | jq -r '.trigger.policy_name')

    CHANGE_COUNT=$(echo "$entry" | jq '[.diff.operations[].field_changes[] | select(.change_kind != "unchanged")] | length')

    if [[ "$CHANGE_COUNT" -lt 1 ]]; then
        echo "FAIL: 401-dhcpv4-all-interfaces-get-changes: lease_acquired entry #$ENTRY_IDX (policy=$POLICY_NAME) has no field changes" >&2
        echo "      The address was likely applied as a side effect of another interface's event." >&2
        echo "      entry:" >&2
        echo "$entry" | jq . >&2
        exit 1
    fi
done <<< "$ACQUIRED_ENTRIES"

echo "PASS: 401-dhcpv4-all-interfaces-get-changes"
