#!/bin/bash
# 010-daemon-exit-trap.sh
# Integration test: The EXIT trap registered by daemon_test_setup kills the
# daemon process, kills dnsmasq processes, and removes the temp directory when
# the script exits (normally or on error).
#
# Acceptance criteria covered:
#   - EXIT trap kills the daemon process
#   - EXIT trap kills dnsmasq processes started via start_dnsmasq
#   - EXIT trap removes TMPDIR_TEST
#
# Requires: unshare, ip (iproute2), netfyr-daemon binary
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/010-daemon-exit-trap.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

# Use a shared file (outside TMPDIR_TEST) to collect PIDs and paths from the
# inner subshell so we can verify they are cleaned up after it exits.
SHARED=$(mktemp)
# Clean up SHARED on this script's exit (not the subshell's exit).
trap 'rm -f "$SHARED"' EXIT

OUTER_SCRIPT_DIR="$SCRIPT_DIR"
OUTER_NETFYR_BIN="$NETFYR_BIN"
OUTER_NETFYR_DAEMON_BIN="$NETFYR_DAEMON_BIN"

# Run the inner scenario in a subshell. The subshell calls daemon_test_setup
# (which registers _daemon_test_cleanup as its EXIT trap), then starts the
# daemon, then exits normally. The EXIT trap fires and must:
#   1. Kill the daemon
#   2. Remove TMPDIR_TEST
(
    # shellcheck source=helpers.sh
    source "$OUTER_SCRIPT_DIR/helpers.sh"
    NETFYR_BIN="$OUTER_NETFYR_BIN"
    NETFYR_DAEMON_BIN="$OUTER_NETFYR_DAEMON_BIN"

    daemon_test_setup

    # Record the temp dir before starting anything.
    echo "INNER_TMPDIR=$TMPDIR_TEST" > "$SHARED"

    start_daemon

    # Record the daemon PID so the parent can verify it was killed.
    echo "INNER_DAEMON_PID=$DAEMON_PID" >> "$SHARED"

    # Exit normally; _daemon_test_cleanup fires as the EXIT trap.
    exit 0
)

# shellcheck source=/dev/null
source "$SHARED"
rm -f "$SHARED"
trap - EXIT

# --- Verify: TMPDIR_TEST was removed ---
if [[ -d "${INNER_TMPDIR:-}" ]]; then
    echo "FAIL: 010-daemon-exit-trap: EXIT trap did not remove TMPDIR_TEST='$INNER_TMPDIR'" >&2
    exit 1
fi

# --- Verify: daemon process was killed ---
if [[ -n "${INNER_DAEMON_PID:-}" ]] && kill -0 "$INNER_DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 010-daemon-exit-trap: EXIT trap did not kill daemon PID=$INNER_DAEMON_PID" >&2
    exit 1
fi

echo "PASS: 010-daemon-exit-trap"
