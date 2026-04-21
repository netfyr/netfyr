#!/bin/bash
# 600-e2e-external-change.sh -- End-to-end: daemon detects external network changes and journals them.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-external-change.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-external-change: 'jq' not found; install jq to run this test" >&2
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

create_veth veth-e2e0 veth-e2e1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-external-change: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-external-change: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Initial apply: establish managed state (mtu=1400) ────────────────────────

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-external-change
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-external-change: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record the initial count of policy_apply entries so we can assert no new
# policy applies occur during the external-change phases.
INITIAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Phase 1: External MTU change ─────────────────────────────────────────────

ip link set veth-e2e0 mtu 1500

# Wait for the debounce window (500ms) to fire and the journal entry to appear.
sleep 1

EC_COUNT_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_1" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change: expected >= 1 external_change entry after MTU change, got $EC_COUNT_1" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the latest external_change entry has a diff referencing veth-e2e0 with an mtu field change.
EC_ENTRY_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

MTU_CHANGE_COUNT=$(echo "$EC_ENTRY_1" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-e2e0") |
     .field_changes[]? |
     select(.field_name == "mtu")] | length')
if [[ "$MTU_CHANGE_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change: external_change diff does not contain mtu field change for veth-e2e0" >&2
    echo "      entry: $EC_ENTRY_1" >&2
    exit 1
fi

# Verify the daemon did not re-reconcile (mtu stays at 1500, not reverted to 1400).
assert_mtu veth-e2e0 1500

# ── Phase 2: External address additions ──────────────────────────────────────

ip addr add 10.99.0.1/24 dev veth-e2e0
ip addr add 10.99.0.2/24 dev veth-e2e0

sleep 1

EC_COUNT_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_2" -lt 2 ]]; then
    echo "FAIL: 600-e2e-external-change: expected >= 2 external_change entries after address additions, got $EC_COUNT_2" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the latest external_change entry references veth-e2e0.
EC_ENTRY_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

ADDR_OPS_COUNT=$(echo "$EC_ENTRY_2" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-e2e0")] | length')
if [[ "$ADDR_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change: latest external_change diff does not reference veth-e2e0 for address additions" >&2
    echo "      entry: $EC_ENTRY_2" >&2
    exit 1
fi

# Verify both addresses are present on the interface (daemon did not revert them).
assert_has_address veth-e2e0 10.99.0.1/24
assert_has_address veth-e2e0 10.99.0.2/24

# Verify the daemon did not re-reconcile (mtu stays at 1500).
assert_mtu veth-e2e0 1500

# ── Phase 3: External address removal ────────────────────────────────────────

ip addr del 10.99.0.1/24 dev veth-e2e0

sleep 1

EC_COUNT_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_3" -lt 3 ]]; then
    echo "FAIL: 600-e2e-external-change: expected >= 3 external_change entries after address removal, got $EC_COUNT_3" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the latest external_change entry references veth-e2e0.
EC_ENTRY_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

REMOVAL_OPS_COUNT=$(echo "$EC_ENTRY_3" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-e2e0")] | length')
if [[ "$REMOVAL_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change: latest external_change diff does not reference veth-e2e0 for address removal" >&2
    echo "      entry: $EC_ENTRY_3" >&2
    exit 1
fi

# Verify the removed address is gone, and the remaining one is still present.
assert_not_has_address veth-e2e0 10.99.0.1/24
assert_has_address veth-e2e0 10.99.0.2/24

# Verify the daemon did not re-reconcile (mtu stays at 1500).
assert_mtu veth-e2e0 1500

# ── Phase 4: Unmanaged interface ignored ─────────────────────────────────────

create_veth veth-unmanaged0 veth-unmanaged1

# Brief pause so any netlink events from veth creation settle before we
# snapshot the external_change count.
sleep 1

EC_COUNT_BEFORE_UNMANAGED=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# Change MTU on the unmanaged interface (default is 1500, so 1400 is a real change).
ip link set veth-unmanaged0 mtu 1400

# Wait for debounce window to fire (500ms) plus margin.
sleep 1

EC_COUNT_AFTER_UNMANAGED=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_AFTER_UNMANAGED" -ne "$EC_COUNT_BEFORE_UNMANAGED" ]]; then
    echo "FAIL: 600-e2e-external-change: daemon recorded an external_change entry for unmanaged interface" \
         "(before=$EC_COUNT_BEFORE_UNMANAGED, after=$EC_COUNT_AFTER_UNMANAGED)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Secondary defense: no entry should mention veth-unmanaged0.
UNMANAGED_REF_COUNT=$(jq -rs '
    [.[] | select(.trigger.type == "external_change") |
     .diff.operations[]? |
     select(.entity_name == "veth-unmanaged0")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$UNMANAGED_REF_COUNT" -ne 0 ]]; then
    echo "FAIL: 600-e2e-external-change: journal references veth-unmanaged0 ($UNMANAGED_REF_COUNT times)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Final: no new policy_apply entries ───────────────────────────────────────

# The daemon must not have re-applied the original policy in response to any
# of the external changes above.
FINAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$FINAL_APPLY_COUNT" -ne "$INITIAL_APPLY_COUNT" ]]; then
    echo "FAIL: 600-e2e-external-change: daemon re-applied policy during external changes" \
         "(initial=$INITIAL_APPLY_COUNT, final=$FINAL_APPLY_COUNT)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 600-e2e-external-change"
