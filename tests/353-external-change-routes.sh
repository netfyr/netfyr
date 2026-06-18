#!/bin/bash
# 353-external-change-routes.sh -- Daemon detects external route additions and removals.
#
# Verifies acceptance criteria for SPEC-353:
# - Route addition detected and journaled
# - Route removal detected and journaled
# - Multiple routes added at once coalesced into one journal entry
# - Mixed address and route changes produce one journal entry
# - Mixed route additions and removals coalesced into one entry
# - Daemon does not re-apply the original policy during route changes
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/353-external-change-routes.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
setup_journal

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 353-external-change-routes: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

create_veth veth-e2e0 veth-e2e1

start_daemon

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
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 353-external-change-routes: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record initial policy_apply count (daemon must not increase this during route tests).
INITIAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# Add an address externally so that subsequent routes via 10.99.0.254 resolve.
ip addr add 10.99.0.1/24 dev veth-e2e0

# Wait for the address external_change to be recorded.
sleep 1

ADDR_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$ADDR_EC_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change-routes: no external_change entry after address addition" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Establish the baseline external_change count (after address + connected route event).
BASELINE_EC_COUNT=$ADDR_EC_COUNT

# Helper: verify latest external_change has a routes field_change for veth-e2e0
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
        echo "FAIL: 353-external-change-routes: $phase_label: latest external_change entry has no routes field_change for veth-e2e0" >&2
        echo "      entry: $ec_entry" >&2
        exit 1
    fi
}

# ── Phase 1: External route addition ─────────────────────────────────────────
# AC: Monitor detects route addition

ip route add 10.99.1.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_1" -le "$BASELINE_EC_COUNT" ]]; then
    echo "FAIL: 353-external-change-routes: route addition not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "route addition"
BASELINE_EC_COUNT=$EC_COUNT_1

# ── Phase 2: External route removal ──────────────────────────────────────────
# AC: Monitor detects route removal

ip route del 10.99.1.0/24 dev veth-e2e0

sleep 1

EC_COUNT_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_2" -le "$BASELINE_EC_COUNT" ]]; then
    echo "FAIL: 353-external-change-routes: route removal not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_2)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "route removal"
BASELINE_EC_COUNT=$EC_COUNT_2

# ── Phase 3: Multiple routes added at once (debounce coalescing) ──────────────
# AC: Burst changes are coalesced — multiple routes produce one journal entry

ip route add 10.99.2.0/24 via 10.99.0.254 dev veth-e2e0
ip route add 10.99.3.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_3=$(( EC_COUNT_3 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_3" -lt 1 ]]; then
    echo "FAIL: 353-external-change-routes: multiple routes not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_3)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_3" -gt 1 ]]; then
    echo "FAIL: 353-external-change-routes: multiple routes not coalesced into one entry" \
         "(got $NEW_ENTRIES_3 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "multiple routes coalesced"
BASELINE_EC_COUNT=$EC_COUNT_3

# ── Phase 4: Mixed address and route changes ──────────────────────────────────
# AC: Route and address changes are coalesced — produce one journal entry covering both

ip addr add 10.99.0.2/24 dev veth-e2e0
ip route add 10.99.4.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_4=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_4=$(( EC_COUNT_4 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_4" -lt 1 ]]; then
    echo "FAIL: 353-external-change-routes: mixed addr+route not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_4)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_4" -gt 1 ]]; then
    echo "FAIL: 353-external-change-routes: mixed addr+route not coalesced into one entry" \
         "(got $NEW_ENTRIES_4 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "mixed addr+route"
BASELINE_EC_COUNT=$EC_COUNT_4

# ── Phase 5: Mixed route additions and removals ────────────────────────────────
# AC: Mixed additions and removals coalesced into one entry

ip route del 10.99.2.0/24 dev veth-e2e0
ip route add 10.99.5.0/24 via 10.99.0.254 dev veth-e2e0

sleep 1

EC_COUNT_5=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
NEW_ENTRIES_5=$(( EC_COUNT_5 - BASELINE_EC_COUNT ))
if [[ "$NEW_ENTRIES_5" -lt 1 ]]; then
    echo "FAIL: 353-external-change-routes: mixed route add+del not detected" \
         "(before=$BASELINE_EC_COUNT, after=$EC_COUNT_5)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi
if [[ "$NEW_ENTRIES_5" -gt 1 ]]; then
    echo "FAIL: 353-external-change-routes: mixed route add+del not coalesced into one entry" \
         "(got $NEW_ENTRIES_5 new entries, expected 1)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

assert_routes_field_change "mixed route add+del"

# ── Final: daemon must not have re-applied the original policy ─────────────────
# AC: Daemon does not re-apply policy after external changes

FINAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$FINAL_APPLY_COUNT" -ne "$INITIAL_APPLY_COUNT" ]]; then
    echo "FAIL: 353-external-change-routes: daemon re-applied policy during external route changes" \
         "(initial=$INITIAL_APPLY_COUNT, final=$FINAL_APPLY_COUNT)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 353-external-change-routes"
