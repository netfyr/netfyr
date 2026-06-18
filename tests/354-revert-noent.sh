#!/bin/bash
# 354-revert-noent.sh -- Revert to nonexistent entry fails with a clear error.
#
# Spec test 24: netfyr revert 9999 exits 1 with an error mentioning "not found".
#
# Requires: unshare
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/354-revert-noent.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 354-revert-noent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

JOURNAL_DIR="$TMPDIR_TEST/journal"
# Point socket at a nonexistent path to force daemon-free mode.
FAKE_SOCKET="$TMPDIR_TEST/no-daemon.sock"
mkdir -p "$JOURNAL_DIR"

# AC: run revert for a seq that does not exist in an empty journal.
REVERT_OUTPUT=""
REVERT_EXIT=0
REVERT_OUTPUT=$(NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert 9999 2>&1) || REVERT_EXIT=$?

# AC: exit code is 1.
if [[ $REVERT_EXIT -ne 1 ]]; then
    echo "FAIL: 354-revert-noent: expected exit code 1, got $REVERT_EXIT" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

# AC: the output contains "not found".
if ! echo "$REVERT_OUTPUT" | grep -qi "not found"; then
    echo "FAIL: 354-revert-noent: output should contain 'not found'" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

echo "PASS: 354-revert-noent"
