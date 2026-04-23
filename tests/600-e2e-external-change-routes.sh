#!/bin/bash
# 600-e2e-external-change-routes.sh -- End-to-end: daemon detects external route changes.
#
# Covers scenario 26 sub-scenarios (SPEC-600) not covered by 600-e2e-external-change.sh:
#   - Route addition detection
#   - Route removal detection
#   - Multiple routes added at once coalesced into one journal entry
#   - Mixed address and route changes produce one journal entry
#   - Mixed route additions and removals produce one journal entry
#   - Daemon does not re-apply the original policy during route changes
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-external-change-routes.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-routes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-routes: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-external-change-routes: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-external-change-routes: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-external-change-routes: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Initial apply: establish managed state ────────────────────────────────────

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-ext-change-routes
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
    echo "FAIL: 600-e2e-external-change-routes: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record initial policy_apply count (daemon must not increase this during route tests).
INITIAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# Add an address externally so that subsequent explicit routes via 10.99.0.254 resolve.
# This also installs a connected route 10.99.0.0/24 on veth-e2e0.
ip addr add 10.99.0.1/24 dev veth-e2e0

# Wait for the address (and connected route) external_change to be recorded.
sleep 1

ADDR_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$ADDR_EC_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: no external_change entry after address addition" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Establish the baseline external_change count (after address + connected route event).
BASELINE_EC_COUNT=$ADDR_EC_COUNT

# ── Helper: verify latest external_change has a routes field_change for veth-e2e0 ──

assert_routes_field_change() {
    local phase_label="$1"
    local ec_entry
    ec_entry=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
        "$JOURNAL_DIR/current.ndjson")
    local routes_change_count
    routes_change_count=$(echo "$ec_entry" | jq '
        [.diff.operations[]? |
         select(.entity_name == "veth-e2e0") |
         .field_changes[]? |
         select(.field_name == "routes")] | length')
    if [[ "$routes_change_count" -lt 1 ]]; then
        echo "FAIL: 600-e2e-external-change-routes: $phase_label: latest external_change entry has no routes field_change for veth-e2e0" >&2
        echo "      entry: $ec_entry" >&2
        exit 1
    fi
}

# ── Phase 1: External route addition ─────────────────────────────────────────
# Scenario: Daemon detects external route addition.
# The gateway 10.99.0.254 is in the connected 10.99.0.0/24 subnet.

ip route add 10.99.1.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_1" -le "$BASELINE_EC_COUNT" ]]; then
    echo "FAIL: 600-e2e-external-change-routes: route addition not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "route addition"
BASELINE_EC_COUNT=$EC_COUNT_1

# ── Phase 2: External route removal ──────────────────────────────────────────
# Scenario: Daemon detects external route removal.

ip route del 10.99.1.0/24 dev veth-e2e0

sleep 1

EC_COUNT_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_2" -le "$BASELINE_EC_COUNT" ]]; then
    echo "FAIL: 600-e2e-external-change-routes: route removal not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_2)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "route removal"
BASELINE_EC_COUNT=$EC_COUNT_2

# ── Phase 3: Multiple routes added at once (debounce coalescing) ──────────────
# Scenario: Multiple routes added in quick succession must produce exactly one
# journal entry (the 500ms sliding debounce window coalesces them).

ip route add 10.99.2.0/24 via 10.99.0.254 dev veth-e2e0
ip route add 10.99.3.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_3=$(( EC_COUNT_3 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_3" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: multiple routes not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_3)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_3" -gt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: multiple routes not coalesced into one entry" \
         "(got $NEW_ENTRIES_3 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "multiple routes coalesced"
BASELINE_EC_COUNT=$EC_COUNT_3

# ── Phase 4: Mixed address and route changes ──────────────────────────────────
# Scenario: An address addition and a route addition in quick succession must
# produce exactly one journal entry covering both changes.

ip addr add 10.99.0.2/24 dev veth-e2e0
ip route add 10.99.4.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_4=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_4=$(( EC_COUNT_4 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_4" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: mixed addr+route not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_4)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_4" -gt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: mixed addr+route not coalesced into one entry" \
         "(got $NEW_ENTRIES_4 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "mixed addr+route"
BASELINE_EC_COUNT=$EC_COUNT_4

# ── Phase 5: Mixed route additions and removals ────────────────────────────────
# Scenario: A route removal and a route addition in quick succession must
# produce exactly one journal entry.

ip route del 10.99.2.0/24 dev veth-e2e0
ip route add 10.99.5.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_5=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_5=$(( EC_COUNT_5 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_5" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: mixed route add+del not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_5)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_5" -gt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-routes: mixed route add+del not coalesced into one entry" \
         "(got $NEW_ENTRIES_5 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "mixed route add+del"

# ── Final: daemon must not have re-applied the original policy ─────────────────

FINAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$FINAL_APPLY_COUNT" -ne "$INITIAL_APPLY_COUNT" ]]; then
    echo "FAIL: 600-e2e-external-change-routes: daemon re-applied policy during external route changes" \
         "(initial=$INITIAL_APPLY_COUNT, final=$FINAL_APPLY_COUNT)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 600-e2e-external-change-routes"
