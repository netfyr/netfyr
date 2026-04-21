#!/bin/bash
# 600-e2e-addr-idempotent.sh -- End-to-end: re-applying the same addresses produces no duplicates.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-addr-idempotent.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-idempotent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-idempotent: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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
        echo "FAIL: 600-e2e-addr-idempotent: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-addr-idempotent: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a policy with 5 addresses.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-addr-idempotent
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  addresses:
    - "10.99.0.1/24"
    - "10.99.0.2/24"
    - "10.99.0.3/24"
    - "10.99.0.4/24"
    - "10.99.0.5/24"
EOF

# ── First apply ───────────────────────────────────────────────────────────────

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-addr-idempotent: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_has_address veth-addr0 "10.99.0.4/24"
assert_has_address veth-addr0 "10.99.0.5/24"
assert_address_count veth-addr0 5

# ── Second apply (same policy) ────────────────────────────────────────────────

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-addr-idempotent: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Addresses must still be present and count must still be exactly 5 (no duplicates).
assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_has_address veth-addr0 "10.99.0.4/24"
assert_has_address veth-addr0 "10.99.0.5/24"
assert_address_count veth-addr0 5

# Each specific address must appear exactly once in ip addr show output.
ADDR_OUTPUT=$(ip addr show dev veth-addr0 2>&1)
for i in 1 2 3 4 5; do
    addr="10.99.0.$i/24"
    count=$(echo "$ADDR_OUTPUT" | grep -c "$addr") || count=0
    if [[ "$count" -ne 1 ]]; then
        echo "FAIL: 600-e2e-addr-idempotent: address '$addr' appears $count time(s), expected exactly 1" >&2
        echo "      ip addr output: $ADDR_OUTPUT" >&2
        exit 1
    fi
done

# Verify ordering is preserved via netfyr query -o json.
QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query -s name=veth-addr0 -o json 2>&1)
assert_json_address_order "$QUERY_OUTPUT" \
    "10.99.0.1/24" "10.99.0.2/24" "10.99.0.3/24" "10.99.0.4/24" "10.99.0.5/24"

echo "PASS: 600-e2e-addr-idempotent"
