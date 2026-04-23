#!/bin/bash
# 600-e2e-external-change-after-restart.sh -- End-to-end: daemon detects external address
# changes after a restart, without any preceding RTM_NEWLINK event for the interface.
#
# Acceptance criteria: when the daemon restarts (without any link attribute changes occurring),
# it must still be able to detect address changes on managed interfaces. This verifies the
# startup RTM_GETLINK dump (dump_link_names) which pre-populates the netlink monitor's
# name cache so that RTM_NEWADDR messages can be resolved to interface names even without
# a preceding RTM_NEWLINK.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-external-change-after-restart.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-external-change-after-restart: 'jq' not found; install jq to run this test" >&2
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

# Helper: wait for the daemon socket to appear (up to 5 seconds).
wait_for_socket() {
    local waited=0
    while [[ ! -S "$SOCKET_PATH" ]]; do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            echo "FAIL: 600-e2e-external-change-after-restart: daemon exited before socket appeared" >&2
            exit 1
        fi
        if (( waited >= 50 )); then
            echo "FAIL: 600-e2e-external-change-after-restart: daemon socket did not appear within 5 seconds" >&2
            exit 1
        fi
        sleep 0.1
        (( waited++ )) || true
    done
}

# ── First daemon start ────────────────────────────────────────────────────────

NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

wait_for_socket

# Apply a policy that manages veth-e2e0. This establishes a journal snapshot
# for veth-e2e0, which the restarted daemon will use as the baseline for
# external change detection.
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
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Record the journal entry count after the initial apply.
INITIAL_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Restart the daemon ────────────────────────────────────────────────────────
# Stop the daemon gracefully and remove the stale socket. The veth interfaces
# remain in place, so no RTM_NEWLINK events will fire for veth-e2e0 when the
# new daemon starts. The only way the new daemon can resolve RTM_NEWADDR events
# for veth-e2e0 is via the RTM_GETLINK dump performed during NetlinkMonitor::start().

kill "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

rm -f "$SOCKET_PATH"

# Restart with the same policy dir (policies persist on disk) and journal dir.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

wait_for_socket

# Allow the startup reconciliation (DaemonStartup trigger) to complete and
# write its journal entry before we trigger the external change.
sleep 0.5

# ── External address addition (after restart, no preceding NEWLINK) ───────────
# This RTM_NEWADDR event arrives with only an ifindex. The monitor must resolve
# the ifname from its startup name cache (populated by the RTM_GETLINK dump),
# not from a preceding RTM_NEWLINK. If dump_link_names failed, the monitor
# would have no mapping for veth-e2e0's ifindex, and the event would be dropped.

ip addr add 10.99.0.1/24 dev veth-e2e0

# Wait for the 500ms debounce window to fire plus a safety margin.
sleep 1.5

# ── Assert: an external_change entry was recorded ─────────────────────────────

FINAL_EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$FINAL_EC_COUNT" -le "$INITIAL_EC_COUNT" ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: no external_change entry recorded after" \
         "address addition (before=$INITIAL_EC_COUNT, after=$FINAL_EC_COUNT)." \
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
    echo "FAIL: 600-e2e-external-change-after-restart: latest external_change entry does not" \
         "include veth-e2e0 in changed_entities" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# The diff must include a field_change for veth-e2e0 (the address addition).
DIFF_OPS_COUNT=$(echo "$LATEST_EC" | jq '
    [.diff.operations[]? | select(.entity_name == "veth-e2e0")] | length')
if [[ "$DIFF_OPS_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: latest external_change diff does not" \
         "include an operation for veth-e2e0" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# The outcome must be "observed" (no re-reconciliation).
OUTCOME_KIND=$(echo "$LATEST_EC" | jq -r '.outcome.kind')
if [[ "$OUTCOME_KIND" != "observed" ]]; then
    echo "FAIL: 600-e2e-external-change-after-restart: expected outcome 'observed', got '$OUTCOME_KIND'" >&2
    echo "      entry: $LATEST_EC" >&2
    exit 1
fi

# The address must still be present on the interface (daemon did not revert it).
assert_has_address veth-e2e0 10.99.0.1/24

echo "PASS: 600-e2e-external-change-after-restart"
