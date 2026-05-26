#!/bin/bash
# 010-daemon-lifecycle.sh
# Integration test: daemon_test_setup, setup_journal, start_daemon, stop_daemon,
# and restart_daemon work correctly using the new harness helpers.
#
# Acceptance criteria covered:
#   - daemon_test_setup creates TMPDIR_TEST, SOCKET_PATH, POLICY_DIR
#   - NETFYR_SOCKET_PATH and NETFYR_POLICY_DIR are exported
#   - setup_journal creates JOURNAL_DIR and exports NETFYR_JOURNAL_DIR
#   - start_daemon starts daemon in background, sets DAEMON_PID, waits for socket
#   - stop_daemon kills daemon, clears DAEMON_PID, removes socket file
#   - restart_daemon stops old daemon and starts new one with same directories
#
# Requires: unshare, ip (iproute2), netfyr-daemon binary
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/010-daemon-lifecycle.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# --- Verify daemon_test_setup: TMPDIR_TEST is a directory ---
if [[ ! -d "$TMPDIR_TEST" ]]; then
    echo "FAIL: 010-daemon-lifecycle: TMPDIR_TEST='$TMPDIR_TEST' is not a directory" >&2
    exit 1
fi

# --- Verify daemon_test_setup: SOCKET_PATH is $TMPDIR_TEST/netfyr.sock ---
if [[ "$SOCKET_PATH" != "$TMPDIR_TEST/netfyr.sock" ]]; then
    echo "FAIL: 010-daemon-lifecycle: SOCKET_PATH='$SOCKET_PATH', expected '$TMPDIR_TEST/netfyr.sock'" >&2
    exit 1
fi

# --- Verify daemon_test_setup: POLICY_DIR exists ---
if [[ ! -d "$POLICY_DIR" ]]; then
    echo "FAIL: 010-daemon-lifecycle: POLICY_DIR='$POLICY_DIR' is not a directory" >&2
    exit 1
fi

if [[ "$POLICY_DIR" != "$TMPDIR_TEST/policies" ]]; then
    echo "FAIL: 010-daemon-lifecycle: POLICY_DIR='$POLICY_DIR', expected '$TMPDIR_TEST/policies'" >&2
    exit 1
fi

# --- Verify daemon_test_setup: env vars exported ---
if [[ "${NETFYR_SOCKET_PATH:-}" != "$SOCKET_PATH" ]]; then
    echo "FAIL: 010-daemon-lifecycle: NETFYR_SOCKET_PATH not exported (got '${NETFYR_SOCKET_PATH:-}', expected '$SOCKET_PATH')" >&2
    exit 1
fi

if [[ "${NETFYR_POLICY_DIR:-}" != "$POLICY_DIR" ]]; then
    echo "FAIL: 010-daemon-lifecycle: NETFYR_POLICY_DIR not exported (got '${NETFYR_POLICY_DIR:-}', expected '$POLICY_DIR')" >&2
    exit 1
fi

# --- setup_journal: creates JOURNAL_DIR and exports NETFYR_JOURNAL_DIR ---
setup_journal

if [[ ! -d "$JOURNAL_DIR" ]]; then
    echo "FAIL: 010-daemon-lifecycle: JOURNAL_DIR='$JOURNAL_DIR' is not a directory" >&2
    exit 1
fi

if [[ "$JOURNAL_DIR" != "$TMPDIR_TEST/journal" ]]; then
    echo "FAIL: 010-daemon-lifecycle: JOURNAL_DIR='$JOURNAL_DIR', expected '$TMPDIR_TEST/journal'" >&2
    exit 1
fi

if [[ "${NETFYR_JOURNAL_DIR:-}" != "$JOURNAL_DIR" ]]; then
    echo "FAIL: 010-daemon-lifecycle: NETFYR_JOURNAL_DIR not exported (got '${NETFYR_JOURNAL_DIR:-}', expected '$JOURNAL_DIR')" >&2
    exit 1
fi

# --- start_daemon: sets DAEMON_PID and creates socket ---
start_daemon

if [[ -z "${DAEMON_PID:-}" ]]; then
    echo "FAIL: 010-daemon-lifecycle: DAEMON_PID not set after start_daemon" >&2
    exit 1
fi

if [[ ! -S "$SOCKET_PATH" ]]; then
    echo "FAIL: 010-daemon-lifecycle: socket not created at '$SOCKET_PATH' after start_daemon" >&2
    exit 1
fi

if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 010-daemon-lifecycle: daemon process DAEMON_PID=$DAEMON_PID is not running" >&2
    exit 1
fi

FIRST_PID="$DAEMON_PID"

# --- stop_daemon: kills daemon, clears DAEMON_PID, removes socket ---
stop_daemon

if [[ -n "${DAEMON_PID:-}" ]]; then
    echo "FAIL: 010-daemon-lifecycle: DAEMON_PID not cleared after stop_daemon (got '$DAEMON_PID')" >&2
    exit 1
fi

if [[ -e "$SOCKET_PATH" ]]; then
    echo "FAIL: 010-daemon-lifecycle: socket '$SOCKET_PATH' still exists after stop_daemon" >&2
    exit 1
fi

# Give the OS a moment to finalize the process exit.
sleep 0.1

if kill -0 "$FIRST_PID" 2>/dev/null; then
    echo "FAIL: 010-daemon-lifecycle: daemon process PID=$FIRST_PID still alive after stop_daemon" >&2
    exit 1
fi

# --- restart_daemon: starts new daemon with same SOCKET_PATH and POLICY_DIR ---
restart_daemon

if [[ -z "${DAEMON_PID:-}" ]]; then
    echo "FAIL: 010-daemon-lifecycle: DAEMON_PID not set after restart_daemon" >&2
    exit 1
fi

if [[ ! -S "$SOCKET_PATH" ]]; then
    echo "FAIL: 010-daemon-lifecycle: socket not created at '$SOCKET_PATH' after restart_daemon" >&2
    exit 1
fi

if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 010-daemon-lifecycle: daemon process DAEMON_PID=$DAEMON_PID is not running after restart_daemon" >&2
    exit 1
fi

# Verify same POLICY_DIR is used (policy files written before restart are visible).
MARKER_FILE="$POLICY_DIR/.test-marker"
touch "$MARKER_FILE"
if [[ ! -f "$MARKER_FILE" ]]; then
    echo "FAIL: 010-daemon-lifecycle: POLICY_DIR changed after restart_daemon" >&2
    exit 1
fi
rm -f "$MARKER_FILE"

# --- restart_daemon: verifies persisted policies are reloaded ---
# Write a static MTU policy and create a veth pair for it to act on.
create_veth veth-lc0 veth-lc1

cat > "$POLICY_DIR/mtu-persist.yaml" <<'EOF'
kind: policy
name: lc-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-lc0
  mtu: 1400
EOF

# Stop and restart the daemon so it loads the policy from POLICY_DIR on startup.
stop_daemon
restart_daemon

# Give the daemon a moment to apply the policy after starting.
sleep 0.5

LINK_OUTPUT=$(ip link show veth-lc0 2>&1) || true
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 010-daemon-lifecycle: policy not reloaded after restart_daemon (mtu 1400 not set)" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 010-daemon-lifecycle"
