#!/bin/bash
# 406-non-root-revert-denied.sh
# Integration test: Non-root user is denied for Revert (dry_run=false) but
# allowed for Revert with dry_run=true.
# Mapped to acceptance criteria:
#   "Non-root user calls Revert (no dry_run) → PermissionDenied"
#   "Non-root user calls Revert (dry_run=true) → request processed normally"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/406-non-root-revert-denied.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 406-non-root-revert-denied: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 406-non-root-revert-denied: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Check that unprivileged user namespaces are supported before entering the
# outer namespace (netns_setup will also use unshare).
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-revert-denied: unprivileged user namespaces not available" >&2
    exit 0
fi

# Enter an unprivileged user+network namespace (re-executes this script as uid 0).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Re-check nested user namespace support inside the outer namespace.
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-revert-denied: nested user namespaces not available" >&2
    exit 0
fi

daemon_test_setup
setup_journal
# chmod 755 so the nested non-root process (uid 65534) can traverse the
# directory to reach the socket.
chmod 755 "$TMPDIR_TEST"
create_veth veth-test0 veth-test1
start_daemon

# Use a non-existent sequence number. The authorization check fires before the
# journal lookup, so a non-root revert (dry_run=false) gets PermissionDenied
# before EntryNotFound.
TARGET_SEQ=999999

# ── Non-root revert without --dry-run (must be denied) ───────────────────────

REVERT_ERR=""
REVERT_EXIT=0
REVERT_ERR=$(unshare --user -- "$NETFYR_BIN" revert "$TARGET_SEQ" 2>&1) \
    || REVERT_EXIT=$?

if [[ $REVERT_EXIT -eq 0 ]]; then
    echo "FAIL: 406-non-root-revert-denied: non-root revert succeeded (expected failure)" >&2
    exit 1
fi

if ! echo "$REVERT_ERR" | grep -q "requires root"; then
    echo "FAIL: 406-non-root-revert-denied: expected 'requires root' in stderr for revert, got:" >&2
    echo "      $REVERT_ERR" >&2
    exit 1
fi

# ── Non-root revert with --dry-run (must pass authorization) ─────────────────

DRYRUN_ERR=""
DRYRUN_EXIT=0
DRYRUN_ERR=$(unshare --user -- "$NETFYR_BIN" revert --dry-run "$TARGET_SEQ" 2>&1) \
    || DRYRUN_EXIT=$?

# PermissionDenied would be the failure mode; any other error (e.g. EntryNotFound)
# means authorization succeeded — which is what we require.
if echo "$DRYRUN_ERR" | grep -q "requires root"; then
    echo "FAIL: 406-non-root-revert-denied: non-root revert --dry-run got PermissionDenied" >&2
    echo "      output: $DRYRUN_ERR" >&2
    exit 1
fi

echo "PASS: 406-non-root-revert-denied"
