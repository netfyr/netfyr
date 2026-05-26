#!/bin/bash
# 010-daemon-start-no-socket.sh
# Integration test: start_daemon exits with code 1 and prints a FAIL message
# when the daemon binary exits immediately without creating a socket.
#
# Acceptance criteria covered:
#   - start_daemon exits 1 if socket does not appear within 5 seconds
#
# Requires: unshare
# Usage: bash tests/010-daemon-start-no-socket.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"

# ---------- Inside the namespace ----------

# Create a fake daemon binary that exits immediately without creating a socket.
TMPDIR_OUTER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_OUTER"' EXIT

FAKE_DAEMON="$TMPDIR_OUTER/fake-daemon-no-socket"
printf '#!/bin/bash\nexit 0\n' > "$FAKE_DAEMON"
chmod +x "$FAKE_DAEMON"

OUTER_SCRIPT_DIR="$SCRIPT_DIR"

# Run start_daemon in a subshell so the exit 1 it calls doesn't kill this script.
# The subshell calls daemon_test_setup (creates its own TMPDIR_TEST) and then
# start_daemon. start_daemon polls for 5 seconds, finds no socket, and calls exit 1.
subshell_rc=0
(
    # shellcheck source=helpers.sh
    source "$OUTER_SCRIPT_DIR/helpers.sh"
    NETFYR_DAEMON_BIN="$FAKE_DAEMON"
    daemon_test_setup
    start_daemon 2>/dev/null
) 2>/dev/null || subshell_rc=$?

if [[ "$subshell_rc" -ne 1 ]]; then
    echo "FAIL: 010-daemon-start-no-socket: expected exit code 1 when socket never appears, got $subshell_rc" >&2
    exit 1
fi

echo "PASS: 010-daemon-start-no-socket"
