#!/bin/bash
# 600-e2e-history-separator.sh -- End-to-end: history shows daemon-restart separator between sessions.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-separator.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-separator: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-separator: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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
            echo "FAIL: 600-e2e-history-separator: daemon exited before socket appeared" >&2
            exit 1
        fi
        if (( waited >= 50 )); then
            echo "FAIL: 600-e2e-history-separator: daemon socket did not appear within 5 seconds" >&2
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

# Apply policy A (mtu=1400) in the first daemon session.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-separator
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
    echo "FAIL: 600-e2e-history-separator: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# ── Restart the daemon ────────────────────────────────────────────────────────

kill "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

# Remove stale socket so the new daemon can bind.
rm -f "$SOCKET_PATH"

# Start a new daemon instance with the same persistent policy directory.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

wait_for_socket

# Apply policy B (mtu=1300) in the second daemon session.
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-separator
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
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-history-separator: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Verify history shows separator and daemon-startup trigger ─────────────────

HISTORY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 2>&1)

# The separator line appears after a daemon-startup entry.
if ! echo "$HISTORY_OUTPUT" | grep -qF "daemon restart"; then
    echo "FAIL: 600-e2e-history-separator: output does not contain 'daemon restart' separator" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# The TRIGGER column must show daemon-startup for the daemon lifecycle entry.
if ! echo "$HISTORY_OUTPUT" | grep -qF "daemon-startup"; then
    echo "FAIL: 600-e2e-history-separator: output does not contain 'daemon-startup' trigger" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-separator"
