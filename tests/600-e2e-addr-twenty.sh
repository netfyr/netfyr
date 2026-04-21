#!/bin/bash
# 600-e2e-addr-twenty.sh -- End-to-end: twenty addresses applied and verified in order.
#
# Requires: unshare, ip (iproute2), seq (coreutils)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-addr-twenty.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-twenty: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-twenty: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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
        echo "FAIL: 600-e2e-addr-twenty: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-addr-twenty: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Generate policy YAML with 20 addresses programmatically.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
{
    cat <<'EOF'
kind: policy
name: e2e-addr-twenty
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
EOF
    for i in $(seq 1 20); do
        echo "    - \"10.99.0.$i/24\""
    done
} > "$POLICY_FILE"

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-addr-twenty: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify kernel state: all 20 addresses present.
for i in $(seq 1 20); do
    assert_has_address veth-addr0 "10.99.0.$i/24"
done
assert_address_count veth-addr0 20

# Verify ordering via netfyr query -o json.
QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
# Build the ordered address list and verify positions are monotonically increasing.
prev_offset=-1
for i in $(seq 1 20); do
    addr="10.99.0.$i/24"
    if ! echo "$QUERY_OUTPUT" | grep -qF "$addr"; then
        echo "FAIL: 600-e2e-addr-twenty: address '$addr' not found in JSON output" >&2
        echo "      JSON: $QUERY_OUTPUT" >&2
        exit 1
    fi
    before="${QUERY_OUTPUT%%"$addr"*}"
    offset="${#before}"
    if (( offset <= prev_offset )); then
        echo "FAIL: 600-e2e-addr-twenty: address '$addr' is not in expected position (offset $offset <= previous $prev_offset)" >&2
        echo "      JSON: $QUERY_OUTPUT" >&2
        exit 1
    fi
    prev_offset="$offset"
done

echo "PASS: 600-e2e-addr-twenty"
