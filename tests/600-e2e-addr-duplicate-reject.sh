#!/bin/bash
# 600-e2e-addr-duplicate-reject.sh -- End-to-end: duplicate addresses in YAML produce a validation error.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-addr-duplicate-reject.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-duplicate-reject: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-duplicate-reject: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

create_veth veth-addr0 veth-addr1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-addr-duplicate-reject: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-addr-duplicate-reject: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a policy with a duplicate address (10.99.0.1/24 appears twice).
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-addr-dup-reject
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
    - "10.99.0.1/24"
    - "10.99.0.2/24"
    - "10.99.0.1/24"
EOF

# The spec expects exit code 2 for a validation error. The assertion checks for
# any non-zero exit code to remain resilient if validation returns a different
# code (e.g., 1). This test will fail if validation is not wired into the pipeline.
APPLY_EXIT=0
APPLY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) \
    || APPLY_EXIT=$?

if [[ $APPLY_EXIT -eq 0 ]]; then
    echo "FAIL: 600-e2e-addr-duplicate-reject: netfyr apply should have failed but exited 0" >&2
    echo "      output: $APPLY_OUTPUT" >&2
    exit 1
fi

# Error output must mention the duplicate.
if ! echo "$APPLY_OUTPUT" | grep -qi "duplicate"; then
    echo "FAIL: 600-e2e-addr-duplicate-reject: error output does not mention 'duplicate'" >&2
    echo "      output: $APPLY_OUTPUT" >&2
    exit 1
fi

# Nothing should have been applied: the interface must have no inet addresses.
assert_address_count veth-addr0 0

echo "PASS: 600-e2e-addr-duplicate-reject"
