#!/bin/bash
# 353-external-change.sh -- Daemon detects external network changes and journals them.
#
# Verifies acceptance criteria for SPEC-353:
# - Journal entry recorded with trigger "external_change" on MTU change
# - Entry diff shows old→new MTU value
# - Entry outcome is "observed"
# - changed_entities includes the interface name
# - External address additions and removals detected
# - Daemon does not re-reconcile after external changes (mtu stays changed)
# - Unmanaged interface changes do not produce external_change entries
# - History text output shows inline values for external changes
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/353-external-change.sh
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
    echo "FAIL: 353-external-change: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

create_veth veth-e2e0 veth-e2e1

start_daemon

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
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 353-external-change: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record the initial count of policy_apply entries so we can assert no new
# policy applies occur during the external-change phases.
INITIAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Phase 1: External MTU change ─────────────────────────────────────────────
# AC: Journal entry recorded with trigger "external_change" on MTU change
# AC: Entry diff shows old→new MTU value (1400→1500)
# AC: Entry outcome is "observed"
# AC: changed_entities includes "veth-e2e0"

ip link set veth-e2e0 mtu 1500

# Wait for the debounce window (500ms) to fire and the journal entry to appear.
sleep 1

EC_COUNT_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_1" -lt 1 ]]; then
    echo "FAIL: 353-external-change: expected >= 1 external_change entry after MTU change, got $EC_COUNT_1" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the latest external_change entry structure.
EC_ENTRY_1=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

# AC: Entry diff shows mtu field change for veth-e2e0
MTU_CHANGE_COUNT=$(echo "$EC_ENTRY_1" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-e2e0") |
     .field_changes[]? |
     select(.field_name == "mtu")] | length')
if [[ "$MTU_CHANGE_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change: external_change diff does not contain mtu field change for veth-e2e0" >&2
    echo "      entry: $EC_ENTRY_1" >&2
    exit 1
fi

# AC: Entry outcome is "observed"
OUTCOME_KIND=$(echo "$EC_ENTRY_1" | jq -r '.outcome.kind')
if [[ "$OUTCOME_KIND" != "observed" ]]; then
    echo "FAIL: 353-external-change: expected outcome 'observed', got '$OUTCOME_KIND'" >&2
    echo "      entry: $EC_ENTRY_1" >&2
    exit 1
fi

# AC: changed_entities includes "veth-e2e0"
ENTITY_IN_TRIGGER=$(echo "$EC_ENTRY_1" | jq -r '
    .trigger.changed_entities[]? | select(. == "veth-e2e0")' | wc -l | tr -d ' ')
if [[ "$ENTITY_IN_TRIGGER" -lt 1 ]]; then
    echo "FAIL: 353-external-change: changed_entities does not include veth-e2e0" >&2
    echo "      entry: $EC_ENTRY_1" >&2
    exit 1
fi

# AC: trigger type is "external_change"
TRIGGER_TYPE=$(echo "$EC_ENTRY_1" | jq -r '.trigger.type')
if [[ "$TRIGGER_TYPE" != "external_change" ]]; then
    echo "FAIL: 353-external-change: expected trigger type 'external_change', got '$TRIGGER_TYPE'" >&2
    exit 1
fi

# AC: External changes do not trigger re-reconciliation (mtu stays at 1500)
assert_mtu veth-e2e0 1500

# ── Phase 2: External address additions ──────────────────────────────────────
# AC: Journal entry recorded with trigger "external_change" on address addition
# AC: Entry diff shows the address additions

ip addr add 10.99.0.1/24 dev veth-e2e0
ip addr add 10.99.0.2/24 dev veth-e2e0

sleep 1

EC_COUNT_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_2" -lt 2 ]]; then
    echo "FAIL: 353-external-change: expected >= 2 external_change entries after address additions, got $EC_COUNT_2" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify latest external_change entry references veth-e2e0
EC_ENTRY_2=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")
ADDR_OPS_COUNT=$(echo "$EC_ENTRY_2" | jq '
    [.diff.operations[]? | select(.entity_name == "veth-e2e0")] | length')
if [[ "$ADDR_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change: latest external_change diff does not reference veth-e2e0 for address additions" >&2
    echo "      entry: $EC_ENTRY_2" >&2
    exit 1
fi

# Verify both addresses are present and daemon did not revert them
assert_has_address veth-e2e0 10.99.0.1/24
assert_has_address veth-e2e0 10.99.0.2/24
assert_mtu veth-e2e0 1500

# ── Phase 3: External address removal ────────────────────────────────────────
# AC: Journal entry recorded with trigger "external_change" on address removal
# AC: Entry diff shows the address removal

ip addr del 10.99.0.1/24 dev veth-e2e0

sleep 1

EC_COUNT_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_3" -lt 3 ]]; then
    echo "FAIL: 353-external-change: expected >= 3 external_change entries after address removal, got $EC_COUNT_3" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

EC_ENTRY_3=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")
REMOVAL_OPS_COUNT=$(echo "$EC_ENTRY_3" | jq '
    [.diff.operations[]? | select(.entity_name == "veth-e2e0")] | length')
if [[ "$REMOVAL_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change: latest external_change diff does not reference veth-e2e0 for address removal" >&2
    echo "      entry: $EC_ENTRY_3" >&2
    exit 1
fi

# Verify the removed address is gone, the remaining one still present
assert_not_has_address veth-e2e0 10.99.0.1/24
assert_has_address veth-e2e0 10.99.0.2/24
assert_mtu veth-e2e0 1500

# ── Phase 4: Unmanaged interface ignored ─────────────────────────────────────
# AC: Monitor ignores unmanaged interfaces — no external_change entry for veth-unmanaged0

create_veth veth-unmanaged0 veth-unmanaged1

# Brief pause so any netlink events from veth creation settle.
sleep 1

EC_COUNT_BEFORE_UNMANAGED=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

ip link set veth-unmanaged0 mtu 1400

sleep 1

EC_COUNT_AFTER_UNMANAGED=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_AFTER_UNMANAGED" -ne "$EC_COUNT_BEFORE_UNMANAGED" ]]; then
    echo "FAIL: 353-external-change: daemon recorded an external_change entry for unmanaged interface" \
         "(before=$EC_COUNT_BEFORE_UNMANAGED, after=$EC_COUNT_AFTER_UNMANAGED)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# No entry should reference veth-unmanaged0
UNMANAGED_REF_COUNT=$(jq -rs '
    [.[] | select(.trigger.type == "external_change") |
     .diff.operations[]? |
     select(.entity_name == "veth-unmanaged0")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$UNMANAGED_REF_COUNT" -ne 0 ]]; then
    echo "FAIL: 353-external-change: journal references veth-unmanaged0 ($UNMANAGED_REF_COUNT times)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Final: no new policy_apply entries ───────────────────────────────────────
# AC: External changes do not trigger re-reconciliation

FINAL_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$FINAL_APPLY_COUNT" -ne "$INITIAL_APPLY_COUNT" ]]; then
    echo "FAIL: 353-external-change: daemon re-applied policy during external changes" \
         "(initial=$INITIAL_APPLY_COUNT, final=$FINAL_APPLY_COUNT)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── History text output verification ─────────────────────────────────────────
# AC: CHANGES column shows inline values (mtu 1400→1500, address values)
# AC: TRIGGER column shows "external" for external change entries
# AC: ENTITIES column shows "veth-e2e0" without lifecycle prefix

HISTORY_TEXT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 5 2>&1)

# CHANGES column must show old MTU value (1400) and new MTU value (1500)
if ! echo "$HISTORY_TEXT" | grep -qF "1400"; then
    echo "FAIL: 353-external-change: text history does not contain old MTU value '1400'" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi
if ! echo "$HISTORY_TEXT" | grep -qF "1500"; then
    echo "FAIL: 353-external-change: text history does not contain new MTU value '1500'" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

# TRIGGER column must show "external" for external change entries
if ! echo "$HISTORY_TEXT" | grep -qF "external"; then
    echo "FAIL: 353-external-change: text history does not show 'external' trigger" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

# ENTITIES column must show "veth-e2e0" without lifecycle prefix
if ! echo "$HISTORY_TEXT" | grep -qF "veth-e2e0"; then
    echo "FAIL: 353-external-change: text history does not contain 'veth-e2e0' in ENTITIES" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

# External changes modify existing entities — no "+veth-e2e0" or "-veth-e2e0" prefix
if echo "$HISTORY_TEXT" | grep -qF "+veth-e2e0"; then
    echo "FAIL: 353-external-change: ENTITIES shows '+veth-e2e0' prefix (external changes do not add entities)" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

echo "PASS: 353-external-change"
