#!/bin/bash
# 600-e2e-journal-seq.sh -- End-to-end: journal sequence numbers are monotonically increasing.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-journal-seq.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-journal-seq: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-journal-seq: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-journal-seq: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-journal-seq: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-journal-seq: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply policy A (mtu=1400) ────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-seq-a
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
    echo "FAIL: 600-e2e-journal-seq: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300) ────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-seq-b
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
    echo "FAIL: 600-e2e-journal-seq: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Verify journal has exactly 2 policy_apply entries with increasing seqs ───

APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$APPLY_COUNT" -ne 2 ]]; then
    echo "FAIL: 600-e2e-journal-seq: expected 2 policy_apply entries, found $APPLY_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Extract the seq numbers of the two policy_apply entries (in journal order, oldest first).
SEQ_FIRST=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][0].seq' \
    "$JOURNAL_DIR/current.ndjson")
SEQ_SECOND=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][1].seq' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$SEQ_SECOND" -le "$SEQ_FIRST" ]]; then
    echo "FAIL: 600-e2e-journal-seq: expected seq to increase: first=$SEQ_FIRST second=$SEQ_SECOND" >&2
    exit 1
fi

# Extract and compare timestamps (ISO 8601 strings sort lexicographically).
TS_FIRST=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][0].timestamp' \
    "$JOURNAL_DIR/current.ndjson")
TS_SECOND=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][1].timestamp' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$TS_SECOND" < "$TS_FIRST" ]]; then
    echo "FAIL: 600-e2e-journal-seq: second timestamp is earlier than first: $TS_FIRST > $TS_SECOND" >&2
    exit 1
fi

echo "PASS: 600-e2e-journal-seq"
