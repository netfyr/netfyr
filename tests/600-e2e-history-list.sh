#!/bin/bash
# 600-e2e-history-list.sh -- End-to-end: netfyr history lists entries in reverse order.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-list.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-list: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-list: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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
        echo "FAIL: 600-e2e-history-list: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-list: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply policy A (mtu=1400) ────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-history-list: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300) ────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-b
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
    echo "FAIL: 600-e2e-history-list: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Run history and verify output ────────────────────────────────────────────

HISTORY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 2>&1)

# Verify the header row contains required column names.
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "SEQ"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'SEQ'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "TIMESTAMP"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'TIMESTAMP'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "TRIGGER"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'TRIGGER'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "OUTCOME"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'OUTCOME'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify at least 2 data rows show "apply (" (the text format for policy-apply trigger).
POLICY_APPLY_COUNT=$(echo "$HISTORY_OUTPUT" | grep -c "apply (") || true
if [[ "$POLICY_APPLY_COUNT" -lt 2 ]]; then
    echo "FAIL: 600-e2e-history-list: expected >= 2 policy-apply rows, found $POLICY_APPLY_COUNT" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify reverse chronological order: first policy-apply row has higher seq than second.
SEQ_FIRST=$(echo "$HISTORY_OUTPUT" | grep "apply (" | head -n 1 | awk '{print $1}')
SEQ_SECOND=$(echo "$HISTORY_OUTPUT" | grep "apply (" | sed -n '2p' | awk '{print $1}')

if [[ -z "$SEQ_FIRST" || -z "$SEQ_SECOND" ]]; then
    echo "FAIL: 600-e2e-history-list: could not extract seq numbers from output" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

if [[ "$SEQ_FIRST" -le "$SEQ_SECOND" ]]; then
    echo "FAIL: 600-e2e-history-list: entries not in reverse order: first_seq=$SEQ_FIRST second_seq=$SEQ_SECOND" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify header contains ENTITIES and CHANGES column names.
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "ENTITIES"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'ENTITIES'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$HISTORY_OUTPUT" | head -n 1 | grep -q "CHANGES"; then
    echo "FAIL: 600-e2e-history-list: output header does not contain 'CHANGES'" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify TRIGGER column shows "apply (e2e-history-list-b)" for the most recent entry.
# The most recent entry is the first apply row (highest seq, shown first).
FIRST_APPLY_LINE=$(echo "$HISTORY_OUTPUT" | grep "apply (" | head -n 1)
if ! echo "$FIRST_APPLY_LINE" | grep -qF "apply (e2e-b)"; then
    echo "FAIL: 600-e2e-history-list: most recent entry TRIGGER does not show 'apply (e2e-b)'" >&2
    echo "      first policy-apply line: $FIRST_APPLY_LINE" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify ENTITIES column shows "veth-e2e0" without "+"/"-" prefix (modified, not created/removed).
if ! echo "$HISTORY_OUTPUT" | grep -qF "veth-e2e0"; then
    echo "FAIL: 600-e2e-history-list: output does not contain 'veth-e2e0' in ENTITIES" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if echo "$HISTORY_OUTPUT" | grep -qF "+veth-e2e0"; then
    echo "FAIL: 600-e2e-history-list: ENTITIES column shows '+veth-e2e0' (entity was modified, not added)" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if echo "$HISTORY_OUTPUT" | grep -qF -- "-veth-e2e0"; then
    echo "FAIL: 600-e2e-history-list: ENTITIES column shows '-veth-e2e0' (entity was modified, not removed)" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify CHANGES column for the second apply (policy B, mtu=1300) shows mtu old→new values.
# The first policy-apply row is policy B (most recent, seq=2); check for both MTU values.
if ! echo "$FIRST_APPLY_LINE" | grep -qF "1400"; then
    echo "FAIL: 600-e2e-history-list: CHANGES column for policy B does not contain old mtu '1400'" >&2
    echo "      first policy-apply line: $FIRST_APPLY_LINE" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$FIRST_APPLY_LINE" | grep -qF "1300"; then
    echo "FAIL: 600-e2e-history-list: CHANGES column for policy B does not contain new mtu '1300'" >&2
    echo "      first policy-apply line: $FIRST_APPLY_LINE" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify CHANGES column shows address removal by value (policy A had 10.99.0.1/24; policy B removes it).
if ! echo "$FIRST_APPLY_LINE" | grep -qF "10.99.0.1"; then
    echo "FAIL: 600-e2e-history-list: CHANGES column does not show address removal value '10.99.0.1'" >&2
    echo "      first policy-apply line: $FIRST_APPLY_LINE" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify --absolute-timestamps shows full date format (YYYY-MM-DD).
ABS_HISTORY=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history --absolute-timestamps -n 5 2>&1)
if ! echo "$ABS_HISTORY" | grep -qE "[0-9]{4}-[0-9]{2}-[0-9]{2}"; then
    echo "FAIL: 600-e2e-history-list: --absolute-timestamps output does not match YYYY-MM-DD pattern" >&2
    echo "      output: $ABS_HISTORY" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-list"
