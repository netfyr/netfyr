#!/bin/bash
# 353-external-change-carrier.sh -- Daemon detects carrier state changes as external changes.
#
# Verifies acceptance criteria for SPEC-353:
# - When the carrier state changes externally (e.g., peer link goes down),
#   the daemon records an external_change journal entry with the carrier field change.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/353-external-change-carrier.sh
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
    echo "FAIL: 353-external-change-carrier: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

create_veth veth-car0 veth-car1

start_daemon

# ── Initial apply: establish managed state ────────────────────────────────────

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-carrier
factory: static
priority: 100
state:
  type: ethernet
  name: veth-car0
  mtu: 1500
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 353-external-change-carrier: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Let the journal settle before snapshotting the external_change count.
sleep 1

EC_COUNT_BEFORE=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Drop carrier by bringing the peer end down ───────────────────────────────
# AC: When the carrier state changes externally, the daemon detects and journals it.
# The managed interface (veth-car0) stays admin-up but loses carrier because
# the peer (veth-car1) goes down.

ip link set veth-car1 down

# Wait for debounce (500ms) + processing buffer.
sleep 1.5

EC_COUNT_AFTER=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$EC_COUNT_AFTER" -le "$EC_COUNT_BEFORE" ]]; then
    echo "FAIL: 353-external-change-carrier: carrier drop did not create ExternalChange entry" \
         "(before=$EC_COUNT_BEFORE, after=$EC_COUNT_AFTER)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# AC: Entry diff shows carrier field change for the managed interface
EC_ENTRY=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

CARRIER_CHANGE_COUNT=$(echo "$EC_ENTRY" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-car0") |
     .field_changes[]? |
     select(.field_name == "carrier")] | length')
if [[ "$CARRIER_CHANGE_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-external-change-carrier: ExternalChange diff does not contain carrier field change for veth-car0" >&2
    echo "      entry: $EC_ENTRY" >&2
    exit 1
fi

# AC: Entry outcome is "observed"
OUTCOME_KIND=$(echo "$EC_ENTRY" | jq -r '.outcome.kind')
if [[ "$OUTCOME_KIND" != "observed" ]]; then
    echo "FAIL: 353-external-change-carrier: expected outcome 'observed', got '$OUTCOME_KIND'" >&2
    echo "      entry: $EC_ENTRY" >&2
    exit 1
fi

# AC: History text output shows carrier change
HISTORY_TEXT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 5 2>&1)

if ! echo "$HISTORY_TEXT" | grep -qF "carrier"; then
    echo "FAIL: 353-external-change-carrier: history output does not contain 'carrier' in CHANGES column" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

echo "PASS: 353-external-change-carrier"
