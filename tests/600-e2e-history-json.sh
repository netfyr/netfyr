#!/bin/bash
# 600-e2e-history-json.sh -- End-to-end: netfyr history -o json produces valid JSON.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-json.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-json: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-json: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-json: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-history-json: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-json: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply policy A ────────────────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-history-json-a
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
    echo "FAIL: 600-e2e-history-json: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B ────────────────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-history-json-b
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
    echo "FAIL: 600-e2e-history-json: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Run history -o json --trigger apply ──────────────────────────────────────

HISTORY_JSON=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 -o json --trigger apply 2>&1)

# Verify output is a valid JSON array.
if ! echo "$HISTORY_JSON" | jq 'type == "array"' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-json: output is not valid JSON" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

IS_ARRAY=$(echo "$HISTORY_JSON" | jq 'type == "array"')
if [[ "$IS_ARRAY" != "true" ]]; then
    echo "FAIL: 600-e2e-history-json: output is not a JSON array" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

# Count the policy_apply entries in the output.
APPLY_COUNT=$(echo "$HISTORY_JSON" | jq '[.[] | select(.trigger.type == "policy_apply")] | length')
if [[ "$APPLY_COUNT" -ne 2 ]]; then
    echo "FAIL: 600-e2e-history-json: expected 2 policy_apply entries, found $APPLY_COUNT" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

# Verify each element has the required fields.
ALL_HAVE_FIELDS=$(echo "$HISTORY_JSON" | jq '
    all(
        has("seq") and
        has("timestamp") and
        has("trigger") and
        has("outcome")
    )')
if [[ "$ALL_HAVE_FIELDS" != "true" ]]; then
    echo "FAIL: 600-e2e-history-json: not all elements have seq, timestamp, trigger, outcome fields" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-json"
