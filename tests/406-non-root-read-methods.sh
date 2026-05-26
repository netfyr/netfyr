#!/bin/bash
# 406-non-root-read-methods.sh
# Integration test: Non-root user can call read-only Varlink methods
# (Query, GetShowInfo, GetHistory) without PermissionDenied.
# Mapped to acceptance criteria:
#   "Non-root user calls read-only methods → request processed normally"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/406-non-root-read-methods.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 406-non-root-read-methods: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 406-non-root-read-methods: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Check that unprivileged user namespaces are supported before entering the
# outer namespace (netns_setup will also use unshare).
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-read-methods: unprivileged user namespaces not available" >&2
    exit 0
fi

# Enter an unprivileged user+network namespace (re-executes this script as uid 0).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Re-check nested user namespace support inside the outer namespace.
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-read-methods: nested user namespaces not available" >&2
    exit 0
fi

daemon_test_setup
setup_journal
# chmod 755 so the nested non-root process (uid 65534) can traverse the
# directory to reach the socket.
chmod 755 "$TMPDIR_TEST"
create_veth veth-test0 veth-test1
start_daemon

# Apply a policy as root so there is history to query.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-read-seed
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"

# ── Query (io.netfyr.Query) ───────────────────────────────────────────────────

QUERY_ERR=""
QUERY_EXIT=0
QUERY_OUT=$(unshare --user -- "$NETFYR_BIN" query 2>&1) || QUERY_EXIT=$?

if [[ $QUERY_EXIT -ne 0 ]]; then
    echo "FAIL: 406-non-root-read-methods: non-root query exited with code $QUERY_EXIT" >&2
    echo "      output: $QUERY_OUT" >&2
    exit 1
fi

if echo "$QUERY_OUT" | grep -qi "permission\|requires root"; then
    echo "FAIL: 406-non-root-read-methods: non-root query returned PermissionDenied" >&2
    echo "      output: $QUERY_OUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUT" | grep -q "type:"; then
    echo "FAIL: 406-non-root-read-methods: non-root query output missing 'type:' field" >&2
    echo "      output: $QUERY_OUT" >&2
    exit 1
fi

# ── GetShowInfo (io.netfyr.GetShowInfo) ──────────────────────────────────────

SHOW_EXIT=0
SHOW_OUT=$(unshare --user -- "$NETFYR_BIN" show 2>&1) || SHOW_EXIT=$?

if [[ $SHOW_EXIT -ne 0 ]]; then
    echo "FAIL: 406-non-root-read-methods: non-root show exited with code $SHOW_EXIT" >&2
    echo "      output: $SHOW_OUT" >&2
    exit 1
fi

if echo "$SHOW_OUT" | grep -qi "permission\|requires root"; then
    echo "FAIL: 406-non-root-read-methods: non-root show returned PermissionDenied" >&2
    echo "      output: $SHOW_OUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUT" | grep -q "running"; then
    echo "FAIL: 406-non-root-read-methods: non-root show output missing 'running'" >&2
    echo "      output: $SHOW_OUT" >&2
    exit 1
fi

# ── GetHistory (io.netfyr.GetHistory) ─────────────────────────────────────────

HISTORY_EXIT=0
HISTORY_OUT=$(unshare --user -- "$NETFYR_BIN" history 2>&1) || HISTORY_EXIT=$?

if [[ $HISTORY_EXIT -ne 0 ]]; then
    echo "FAIL: 406-non-root-read-methods: non-root history exited with code $HISTORY_EXIT" >&2
    echo "      output: $HISTORY_OUT" >&2
    exit 1
fi

if echo "$HISTORY_OUT" | grep -qi "permission\|requires root"; then
    echo "FAIL: 406-non-root-read-methods: non-root history returned PermissionDenied" >&2
    echo "      output: $HISTORY_OUT" >&2
    exit 1
fi

if ! echo "$HISTORY_OUT" | grep -qiE "SEQ|TRIGGER|apply"; then
    echo "FAIL: 406-non-root-read-methods: non-root history output looks empty or wrong" >&2
    echo "      output: $HISTORY_OUT" >&2
    exit 1
fi

echo "PASS: 406-non-root-read-methods"
