#!/bin/bash
# 600-e2e-debug-logging.sh -- End-to-end: debug-level log messages cover key daemon event flow.
#
# Requires: unshare, ip (iproute2), grep
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-debug-logging.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-debug-logging: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-debug-logging: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; wait "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
DAEMON_LOG="$TMPDIR_TEST/daemon.log"
mkdir -p "$POLICY_DIR"

create_veth veth-e2e0 veth-e2e1

# Start the daemon with debug logging enabled and stderr captured to a file.
RUST_LOG=netfyr_daemon=debug \
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" 2>"$DAEMON_LOG" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-debug-logging: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-debug-logging: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write and apply a static policy to trigger reconciliation (emits "diff computed").
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-debug-logging
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-debug-logging: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Trigger an external MTU change to exercise the netlink event flow:
# netlink event parsed → debounce timer fired → recording external changes.
ip link set veth-e2e0 mtu 1500

# Wait for the debounce window to expire (daemon uses ~500 ms debounce; 2 s is generous).
sleep 2

# Kill the daemon and wait for it to exit so stderr is fully flushed to DAEMON_LOG.
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

# Assert all 5 required log patterns are present.
check_pattern() {
    local pattern="$1"
    if ! grep -q "$pattern" "$DAEMON_LOG"; then
        echo "FAIL: 600-e2e-debug-logging: pattern \"$pattern\" not found in daemon log" >&2
        echo "      (daemon log contents follow)" >&2
        cat "$DAEMON_LOG" >&2 || true
        exit 1
    fi
}

check_pattern "RTM_GETLINK dump"
check_pattern "netlink event parsed"
check_pattern "debounce timer fired"
check_pattern "recording external changes"
check_pattern "diff computed"

echo "PASS: 600-e2e-debug-logging"
