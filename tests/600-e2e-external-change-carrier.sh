#!/bin/bash
# 600-e2e-external-change-carrier.sh -- End-to-end: daemon detects carrier changes and journals them.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-external-change-carrier.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-carrier: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-external-change-carrier: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-external-change-carrier: 'jq' not found; install jq to run this test" >&2
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

create_veth veth-car0 veth-car1

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
        echo "FAIL: 600-e2e-external-change-carrier: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-external-change-carrier: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Initial apply: establish managed state ───────────────────────────────────

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
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-external-change-carrier: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Let the journal settle.
sleep 1

EC_COUNT_BEFORE=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

# ── Drop carrier by bringing the peer end down ───────────────────────────────
# The managed interface (veth-car0) stays admin-up, but loses carrier because
# the peer (veth-car1) is now down.

ip link set veth-car1 down

# Wait for debounce (500ms) + processing buffer.
sleep 1.5

EC_COUNT_AFTER=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$EC_COUNT_AFTER" -le "$EC_COUNT_BEFORE" ]]; then
    echo "FAIL: 600-e2e-external-change-carrier: carrier drop did not create ExternalChange entry" \
         "(before=$EC_COUNT_BEFORE, after=$EC_COUNT_AFTER)" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the latest ExternalChange entry has a carrier field change.
EC_ENTRY=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | last' \
    "$JOURNAL_DIR/current.ndjson")

CARRIER_CHANGE_COUNT=$(echo "$EC_ENTRY" | jq '
    [.diff.operations[]? |
     select(.entity_name == "veth-car0") |
     .field_changes[]? |
     select(.field_name == "carrier")] | length')
if [[ "$CARRIER_CHANGE_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-external-change-carrier: ExternalChange diff does not contain carrier field change for veth-car0" >&2
    echo "      entry: $EC_ENTRY" >&2
    exit 1
fi

# ── Verify history text output shows carrier change ──────────────────────────

HISTORY_TEXT=$(NO_COLOR=1 \
    NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 5 2>&1)

if ! echo "$HISTORY_TEXT" | grep -qF "carrier"; then
    echo "FAIL: 600-e2e-external-change-carrier: history output does not contain 'carrier' in CHANGES column" >&2
    echo "      output: $HISTORY_TEXT" >&2
    exit 1
fi

echo "PASS: 600-e2e-external-change-carrier"
