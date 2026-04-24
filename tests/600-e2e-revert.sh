#!/bin/bash
# 600-e2e-revert.sh -- End-to-end: netfyr revert restores a previous network state.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-revert.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-revert: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-revert: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-revert: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-revert: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-revert: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply policy A (mtu=1400) ────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-revert-a
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
    "$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Extract the seq of the policy_apply entry for policy A.
SEQ_A=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$SEQ_A" || "$SEQ_A" == "null" ]]; then
    echo "FAIL: 600-e2e-revert: could not find policy_apply entry for policy A" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300) ────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-revert-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1300

# ── Revert to state A (mtu=1400) ─────────────────────────────────────────────

REVERT_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert "$SEQ_A" || REVERT_EXIT=$?
if [[ $REVERT_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert: revert exited with code $REVERT_EXIT" >&2
    exit 1
fi

# Verify the MTU was restored to 1400.
assert_mtu veth-e2e0 1400

# Verify the journal has a new entry with trigger type "revert".
REVERT_COUNT=$(jq -rs '[.[] | select(.trigger.type == "revert")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$REVERT_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-revert: no revert journal entry found after revert" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify history TRIGGER column shows "revert (SEQ_A)" with the target sequence number.
HISTORY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 1 2>&1)
if ! echo "$HISTORY_OUTPUT" | grep -qF "revert ($SEQ_A)"; then
    echo "FAIL: 600-e2e-revert: history TRIGGER does not show 'revert ($SEQ_A)'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-revert"
