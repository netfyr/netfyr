#!/bin/bash
# 356-show-no-daemon.sh -- End-to-end: netfyr show works gracefully when daemon is not running.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 356-show-no-daemon: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# Point socket path at a nonexistent path — daemon is not running.
SOCKET_PATH="$TMPDIR_TEST/nonexistent.sock"

# Create a veth pair so there are real interfaces to list.
create_veth veth-e2e0 veth-e2e1

# Run netfyr show (no daemon running) — expect exit 0 and graceful fallback.
SHOW_EXIT=0
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1) || SHOW_EXIT=$?

if [[ $SHOW_EXIT -ne 0 ]]; then
    echo "FAIL: 356-show-no-daemon: netfyr show exited with code $SHOW_EXIT (expected 0)" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Status: not running.
if ! echo "$SHOW_OUTPUT" | grep -q "Status:  not running"; then
    echo "FAIL: 356-show-no-daemon: show output does not contain 'Status:  not running'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify the created interface appears in the output.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-e2e0"; then
    echo "FAIL: 356-show-no-daemon: show output does not list 'veth-e2e0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify no Policies:, DHCP:, or Lease: lines appear.
if echo "$SHOW_OUTPUT" | grep -q "Policies:"; then
    echo "FAIL: 356-show-no-daemon: show output unexpectedly contains 'Policies:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if echo "$SHOW_OUTPUT" | grep -q "DHCP:"; then
    echo "FAIL: 356-show-no-daemon: show output unexpectedly contains 'DHCP:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if echo "$SHOW_OUTPUT" | grep -q "Lease:"; then
    echo "FAIL: 356-show-no-daemon: show output unexpectedly contains 'Lease:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 356-show-no-daemon"
