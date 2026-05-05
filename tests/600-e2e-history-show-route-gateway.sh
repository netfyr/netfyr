#!/bin/bash
# 600-e2e-history-show-route-gateway.sh -- End-to-end: netfyr history --show
# renders the gateway in the diff when a route has one.
#
# Bug: the detail diff shows "+0.0.0.0/0 metric 100" but omits the gateway
# even though the route has gateway: 10.0.0.254.  Expected output should
# contain "via GATEWAY" in the route diff line.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-show-route-gateway.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-show-route-gateway: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-show-route-gateway: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-show-route-gateway: 'jq' not found; install jq to run this test" >&2
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

create_veth veth-gw0 veth-gw1
add_address veth-gw0 10.0.0.1/24

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-history-show-route-gateway: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-show-route-gateway: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

TEST_NAME="600-e2e-history-show-route-gateway"

# Apply a policy with a route that has a gateway.  Use a non-default
# destination so the kernel can add it without needing ARP resolution
# (connected subnet route via the veth peer).
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: gw-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-gw0
  addresses:
    - "10.0.0.1/24"
  routes:
    - destination: "10.200.0.0/16"
      gateway: "10.0.0.254"
      metric: 100
EOF

# Apply — may fail at kernel level (gateway unreachable in namespace) but
# the journal entry with the diff is still recorded, which is what we test.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" 2>&1 || true

# Find the seq of the policy_apply entry.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: $TEST_NAME: could not find policy_apply entry in journal" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Run history --show and capture.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    NO_COLOR=1 \
    "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# The diff section must contain the gateway for the route.
if ! echo "$SHOW_OUTPUT" | grep -qF "via 10.0.0.254"; then
    echo "FAIL: $TEST_NAME: diff does not contain 'via 10.0.0.254'" >&2
    echo "      expected the route diff line to include the gateway" >&2
    echo "      output:" >&2
    echo "$SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: $TEST_NAME"
