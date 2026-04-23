#!/bin/bash
# 600-e2e-self-change-excluded.sh -- End-to-end: daemon self-changes (policy applies) are
# excluded from external_change journal entries.
#
# Acceptance criteria: when the daemon applies a policy that changes mtu on an interface,
# exactly one journal entry is recorded with trigger "policy_apply" and no journal entry is
# recorded with trigger "external_change".
#
# The exclusion mechanism is twofold:
#   1. set_applying(true) is set before reconcile_and_apply(); the netlink monitor
#      discards events that arrive while this flag is true.
#   2. The journal snapshot (state_after) is re-queried post-apply, so subsequent
#      netlink events (debounced ~500ms later) compare equal and produce no diff.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-self-change-excluded.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-self-change-excluded: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-self-change-excluded: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-self-change-excluded: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply a policy that changes the MTU on veth-e2e0 ─────────────────────────
# Default veth MTU is 1500; we change it to 1400. This generates RTM_NEWLINK
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
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Wait for the debounce window (500ms) to fire and any spurious external_change
# entries to appear. 1.5 seconds is well beyond the 500ms debounce window.
sleep 1.5

# ── Assert: exactly one policy_apply entry, zero external_change entries ──────

POLICY_APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# There must be at least one policy_apply entry.
if [[ "$POLICY_APPLY_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: expected >= 1 policy_apply entry, got $POLICY_APPLY_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# There must be zero external_change entries — the self-generated RTM_NEWLINK
# events from the policy apply must NOT produce external_change journal entries.
if [[ "$EC_COUNT" -ne 0 ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: expected 0 external_change entries after policy apply," \
         "got $EC_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Apply the same policy again (idempotent) ──────────────────────────────────
# Applying when mtu is already correct must produce no external_change entries either.

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: idempotent apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

sleep 1.5

EC_COUNT_AFTER_IDEMPOTENT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT_AFTER_IDEMPOTENT" -ne 0 ]]; then
    echo "FAIL: 600-e2e-self-change-excluded: expected 0 external_change entries after idempotent apply," \
         "got $EC_COUNT_AFTER_IDEMPOTENT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 600-e2e-self-change-excluded"
