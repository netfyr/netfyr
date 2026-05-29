#!/bin/bash
# 409-dhcpv4-journal-per-interface.sh
# Integration test: when two DHCP policies run concurrently on different
# interfaces, journal entries are scoped to the correct interface and address
# range. Validates SPEC-409 journal attribution.
#
# Topology:
#   veth-dhcp0 <──veth──> veth-dhcp1  (dnsmasq 10.99.1.0/24)
#   veth-dhcp2 <──veth──> veth-dhcp3  (dnsmasq 10.99.2.0/24)
#
# Requires: unshare, ip (iproute2), dnsmasq, jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/409-dhcpv4-journal-per-interface.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: jq not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
setup_journal

# ── Create two DHCP interface pairs ──────────────────────────────────────────

setup_dhcp_topology veth-dhcp0 veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120
setup_dhcp_topology veth-dhcp2 veth-dhcp3 10.99.2.1 10.99.2.100 10.99.2.200 120

# ── Start the daemon ─────────────────────────────────────────────────────────

start_daemon

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
    echo "FAIL: 409-dhcpv4-journal-per-interface: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Wait for both leases ────────────────────────────────────────────────────

wait_for_address veth-dhcp0 "10.99.1." 20
wait_for_address veth-dhcp2 "10.99.2." 20

# Give the daemon time to finish reconciliation and journal writes.
sleep 2

# ── Verify journal entries ───────────────────────────────────────────────────

JOURNAL_FILE="$JOURNAL_DIR/current.ndjson"

if [[ ! -f "$JOURNAL_FILE" ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: journal file not found at $JOURNAL_FILE" >&2
    exit 1
fi

# Extract all lease_acquired entries.
ACQUIRED_ENTRIES=$(jq -c \
    'select(.trigger.type == "dhcp_event" and .trigger.event_kind == "lease_acquired")' \
    "$JOURNAL_FILE" 2>/dev/null || true)

ACQUIRED_COUNT=$(echo "$ACQUIRED_ENTRIES" | grep -c '^{' || true)

if [[ "$ACQUIRED_COUNT" -lt 2 ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: expected at least 2 lease_acquired entries, got $ACQUIRED_COUNT" >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_FILE" >&2
    exit 1
fi

# For each lease_acquired entry, verify at most one entity appears in the diff
# and state_after — confirming no cross-interface mixing.
ENTRY_IDX=0
while IFS= read -r entry; do
    ENTRY_IDX=$((ENTRY_IDX + 1))

    DIFF_ENTITY_COUNT=$(echo "$entry" | jq '[.diff.operations[].entity_name] | length')
    if [[ "$DIFF_ENTITY_COUNT" -gt 1 ]]; then
        DIFF_ENTITIES=$(echo "$entry" | jq -r '[.diff.operations[].entity_name] | join(", ")')
        echo "FAIL: 409-dhcpv4-journal-per-interface: lease_acquired entry #$ENTRY_IDX has $DIFF_ENTITY_COUNT entities in diff ($DIFF_ENTITIES), expected at most 1" >&2
        echo "      entry:" >&2
        echo "$entry" | jq . >&2
        exit 1
    fi

    STATE_ENTITY_COUNT=$(echo "$entry" | jq '[.state_after.entities[].selector_name] | length')
    if [[ "$STATE_ENTITY_COUNT" -gt 1 ]]; then
        STATE_ENTITIES=$(echo "$entry" | jq -r '[.state_after.entities[].selector_name] | join(", ")')
        echo "FAIL: 409-dhcpv4-journal-per-interface: lease_acquired entry #$ENTRY_IDX has $STATE_ENTITY_COUNT entities in state_after ($STATE_ENTITIES), expected at most 1" >&2
        echo "      entry:" >&2
        echo "$entry" | jq . >&2
        exit 1
    fi
done <<< "$ACQUIRED_ENTRIES"

# Verify both policies appear across the entries.
IFACE0_FOUND=$(echo "$ACQUIRED_ENTRIES" | jq -r '.trigger.policy_name' | grep -c 'dhcp-iface0' || true)
IFACE2_FOUND=$(echo "$ACQUIRED_ENTRIES" | jq -r '.trigger.policy_name' | grep -c 'dhcp-iface2' || true)

if [[ "$IFACE0_FOUND" -lt 1 ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: no lease_acquired entry for dhcp-iface0" >&2
    exit 1
fi
if [[ "$IFACE2_FOUND" -lt 1 ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: no lease_acquired entry for dhcp-iface2" >&2
    exit 1
fi

# Verify that each entry's address belongs to the correct subnet.
IFACE0_ENTRY=$(echo "$ACQUIRED_ENTRIES" | jq -c 'select(.trigger.policy_name == "dhcp-iface0")' | head -1)
IFACE2_ENTRY=$(echo "$ACQUIRED_ENTRIES" | jq -c 'select(.trigger.policy_name == "dhcp-iface2")' | head -1)

IFACE0_ADDR=$(echo "$IFACE0_ENTRY" | jq -r '[.state_after.entities[].fields | to_entries[] | select(.key == "address") | .value] | first // empty' 2>/dev/null || true)
IFACE2_ADDR=$(echo "$IFACE2_ENTRY" | jq -r '[.state_after.entities[].fields | to_entries[] | select(.key == "address") | .value] | first // empty' 2>/dev/null || true)

if [[ -n "$IFACE0_ADDR" ]] && [[ "$IFACE0_ADDR" != 10.99.1.* ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: dhcp-iface0 entry has address '$IFACE0_ADDR', expected 10.99.1.x" >&2
    exit 1
fi
if [[ -n "$IFACE2_ADDR" ]] && [[ "$IFACE2_ADDR" != 10.99.2.* ]]; then
    echo "FAIL: 409-dhcpv4-journal-per-interface: dhcp-iface2 entry has address '$IFACE2_ADDR', expected 10.99.2.x" >&2
    exit 1
fi

echo "PASS: 409-dhcpv4-journal-per-interface"
