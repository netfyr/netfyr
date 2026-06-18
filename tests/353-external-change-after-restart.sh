#!/bin/bash
# 353-external-change-after-restart.sh -- External change detection resumes after daemon restart.
#
# Verifies acceptance criteria for SPEC-353:
# - When the daemon restarts (without any link attribute changes occurring),
#   it must still detect address changes on managed interfaces.
# - This verifies the startup RTM_GETLINK dump (dump_link_names) which
#   pre-populates the netlink monitor's name cache so RTM_NEWADDR messages
#   can be resolved to interface names without a preceding RTM_NEWLINK.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/353-external-change-after-restart.sh
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
    echo "FAIL: 353-external-change-after-restart: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

create_veth veth-e2e0 veth-e2e1

start_daemon

# ── First apply: establish managed state and journal snapshot ─────────────────

POLICY_FILE="$POLICY_DIR/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-restart-detect
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
    echo "FAIL: 353-external-change-after-restart: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record the external_change count after the initial apply.
INITIAL_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Restart the daemon ────────────────────────────────────────────────────────
# After restart, no RTM_NEWLINK events fire for veth-e2e0 (the veth remains in
# place). The monitor must resolve RTM_NEWADDR events via the startup name cache
# populated by dump_link_names(), not a preceding RTM_NEWLINK.

restart_daemon

# Allow the startup reconciliation to complete.
sleep 0.5

# ── External address addition (after restart, no preceding NEWLINK) ───────────
# AC: Address change detected after daemon restart

ip addr add 10.99.0.1/24 dev veth-e2e0

# Wait for the 500ms debounce window to fire plus a safety margin.
sleep 1.5

# ── Assert: an external_change entry was recorded ─────────────────────────────

FINAL_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$FINAL_EC_COUNT" -le "$INITIAL_EC_COUNT" ]]; then
    echo "FAIL: 353-external-change-after-restart: no external_change entry recorded after" \
         "address addition post-restart (before=$INITIAL_EC_COUNT, after=$FINAL_EC_COUNT)." \
         "This likely means the startup name cache (dump_link_names) is not working." >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# The latest external_change entry must reference veth-e2e0 in changed_entities.
LATEST_EC=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

ENTITY_IN_TRIGGER=$(echo "$LATEST_EC" | jq -r '
    .trigger.changed_entities[]? | select(. == "veth-e2e0")' | wc -l | tr -d ' ')
if [[ "$ENTITY_IN_TRIGGER" -lt 1 ]]; then
    echo "FAIL: 353-external-change-after-restart: latest external_change entry does not" \
         "include veth-e2e0 in changed_entities" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# The diff must include a field_change for veth-e2e0 (the address addition).
DIFF_OPS_COUNT=$(echo "$LATEST_EC" | jq '
    [.diff.operations[]? | select(.entity_name == "veth-e2e0")] | length')
if [[ "$DIFF_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change-after-restart: latest external_change diff does not" \
         "include an operation for veth-e2e0" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# AC: Entry outcome is "observed"
OUTCOME_KIND=$(echo "$LATEST_EC" | jq -r '.outcome.kind')
if [[ "$OUTCOME_KIND" != "observed" ]]; then
    echo "FAIL: 353-external-change-after-restart: expected outcome 'observed', got '$OUTCOME_KIND'" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# The address must still be present on the interface (daemon did not revert it).
assert_has_address veth-e2e0 10.99.0.1/24

echo "PASS: 353-external-change-after-restart"
