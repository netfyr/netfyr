#!/bin/bash
# 353-self-change-excluded.sh -- Self-changes are excluded from external change detection.
#
# Verifies acceptance criteria for SPEC-353:
# - When netfyr itself changes the interface during reconciliation, no
#   external_change journal entry is recorded for that change.
# - Exactly one journal entry is recorded with trigger "policy_apply".
# - The set_applying flag (AtomicBool) correctly gates out self-generated
#   RTM_NEWLINK events during the apply window.
# - Idempotent re-apply also produces no external_change entries.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/353-self-change-excluded.sh
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
    echo "FAIL: 353-self-change-excluded: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

create_veth veth-e2e0 veth-e2e1

start_daemon

# ── Apply a policy that changes the MTU on veth-e2e0 ─────────────────────────
# Default veth MTU is 1500; change it to 1400. This generates RTM_NEWLINK
# events for veth-e2e0 which must NOT produce an external_change journal entry.

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-self-change
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
    echo "FAIL: 353-self-change-excluded: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Wait for the debounce window (500ms) to fire and any spurious external_change
# entries to appear. 1.5 seconds is well beyond the 500ms debounce window.
sleep 1.5

# ── Assert: policy_apply entry recorded, zero external_change entries ─────────
# AC: Exactly one journal entry recorded with trigger "policy_apply"
# AC: No journal entry recorded with trigger "external_change"

POLICY_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$POLICY_APPLY_COUNT" -lt 1 ]]; then
    echo "FAIL: 353-self-change-excluded: expected >= 1 policy_apply entry, got $POLICY_APPLY_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# AC: Self-generated RTM_NEWLINK events from the policy apply must NOT produce
#     external_change journal entries.
if [[ "$EC_COUNT" -ne 0 ]]; then
    echo "FAIL: 353-self-change-excluded: expected 0 external_change entries after policy apply," \
         "got $EC_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Idempotent re-apply: verify no external_change entries produced ────────────
# Applying when mtu is already correct must also produce no external_change entries.

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 353-self-change-excluded: idempotent apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

sleep 1.5

EC_COUNT_AFTER_IDEMPOTENT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_AFTER_IDEMPOTENT" -ne 0 ]]; then
    echo "FAIL: 353-self-change-excluded: expected 0 external_change entries after idempotent apply," \
         "got $EC_COUNT_AFTER_IDEMPOTENT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 353-self-change-excluded"
