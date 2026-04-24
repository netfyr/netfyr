#!/bin/bash
# 600-e2e-diagnose-drift.sh -- End-to-end: diagnose detects configuration drift after external MTU change.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-diagnose-drift.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-diagnose-drift: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-diagnose-drift: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

create_veth veth-e2e0 veth-e2e1

# Start the daemon with a temp journal directory.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-diagnose-drift: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-diagnose-drift: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write and apply a static policy: mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-diagnose-drift
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-diagnose-drift: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Externally change mtu to 1500 to create drift.
ip link set veth-e2e0 mtu 1500

# Wait for debounce window to expire (~500 ms; 1 s is generous).
sleep 1

# Run diagnose with selector; capture output and exit code.
DIAGNOSE_EXIT=0
DIAGNOSE_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" diagnose -s name=veth-e2e0 2>&1) || DIAGNOSE_EXIT=$?

# Verify: output mentions "configuration drift".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "configuration drift"; then
    echo "FAIL: 600-e2e-diagnose-drift: output does not mention 'configuration drift'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify: output mentions "warning".
if ! echo "$DIAGNOSE_OUTPUT" | grep -qi "warning"; then
    echo "FAIL: 600-e2e-diagnose-drift: output does not mention 'warning'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify: output mentions the policy value (1400).
if ! echo "$DIAGNOSE_OUTPUT" | grep -q "1400"; then
    echo "FAIL: 600-e2e-diagnose-drift: output does not mention policy value 1400" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify: output mentions the system value (1500).
if ! echo "$DIAGNOSE_OUTPUT" | grep -q "1500"; then
    echo "FAIL: 600-e2e-diagnose-drift: output does not mention system value 1500" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify: suggested action mentions "netfyr apply".
if ! echo "$DIAGNOSE_OUTPUT" | grep -q "netfyr apply"; then
    echo "FAIL: 600-e2e-diagnose-drift: output does not suggest 'netfyr apply'" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify: exit code is 1 (warning).
if [[ $DIAGNOSE_EXIT -ne 1 ]]; then
    echo "FAIL: 600-e2e-diagnose-drift: expected exit code 1 (warning), got $DIAGNOSE_EXIT" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-diagnose-drift"
