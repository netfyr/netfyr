#!/bin/bash
# 403-query-via-daemon.sh
# Integration test: `netfyr query` routed through the daemon returns
# current system state for a specific interface.
# Mapped to acceptance criteria:
#   "Query returns current system state"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-query-via-daemon.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-query-via-daemon: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-query-via-daemon: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create a veth pair and apply a known MTU so the query result is predictable.
create_veth veth-test0 veth-test1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 403-query-via-daemon: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-query-via-daemon: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a static policy so the daemon manages veth-test0 and sets mtu=1400.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-query-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE"
APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-query-via-daemon: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify mtu was applied by the daemon.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 403-query-via-daemon: apply did not set mtu 1400 on veth-test0" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

# Query the interface via the daemon. The query returns current system state
# as observed by the netfyr backend.
QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query -s name=veth-test0 2>&1) \
    || QUERY_EXIT=$?

if [[ -z "$QUERY_OUTPUT" ]]; then
    echo "FAIL: 403-query-via-daemon: netfyr query returned empty output" >&2
    exit 1
fi

# The query response must mention the interface name.
if ! echo "$QUERY_OUTPUT" | grep -q "veth-test0"; then
    echo "FAIL: 403-query-via-daemon: query output does not mention veth-test0" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

# The query response must reflect the current system mtu (1400, as applied).
if ! echo "$QUERY_OUTPUT" | grep -q "1400"; then
    echo "FAIL: 403-query-via-daemon: query output does not show mtu 1400" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

# Query without a selector (type=ethernet) must also return data for the
# managed interface — the daemon queries the full backend.
QUERY_ALL_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query 2>&1) || true
if [[ -z "$QUERY_ALL_OUTPUT" ]]; then
    echo "FAIL: 403-query-via-daemon: query with no selector returned empty output" >&2
    exit 1
fi

# The global query must include veth-test0.
if ! echo "$QUERY_ALL_OUTPUT" | grep -q "veth-test0"; then
    echo "FAIL: 403-query-via-daemon: global query does not include veth-test0" >&2
    echo "      query output: $QUERY_ALL_OUTPUT" >&2
    exit 1
fi

echo "PASS: 403-query-via-daemon"
