#!/bin/bash
# 600-e2e-addr-removal.sh -- End-to-end: replacing a policy with one that has no addresses removes them.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-addr-removal.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-removal: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-addr-removal: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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
        echo "FAIL: 600-e2e-addr-removal: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-addr-removal: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Phase 1: Apply policy with mtu=1400 and 3 addresses ──────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-addr-removal
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
    - "10.99.0.2/24"
    - "10.99.0.3/24"
EOF

APPLY_A_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_A" || APPLY_A_EXIT=$?
if [[ $APPLY_A_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-addr-removal: first apply exited with code $APPLY_A_EXIT" >&2
    exit 1
fi

assert_mtu veth-addr0 1400
assert_has_address veth-addr0 "10.99.0.1/24"
assert_has_address veth-addr0 "10.99.0.2/24"
assert_has_address veth-addr0 "10.99.0.3/24"
assert_address_count veth-addr0 3

# ── Phase 2: Replace with a policy that has mtu=1400 but no addresses ─────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-addr-removal
factory: static
priority: 100
state:
  type: ethernet
  name: veth-addr0
  mtu: 1400
EOF

APPLY_B_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_B" || APPLY_B_EXIT=$?
if [[ $APPLY_B_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-addr-removal: second apply exited with code $APPLY_B_EXIT" >&2
    exit 1
fi

# All 3 addresses must be gone.
assert_not_has_address veth-addr0 "10.99.0.1/24"
assert_not_has_address veth-addr0 "10.99.0.2/24"
assert_not_has_address veth-addr0 "10.99.0.3/24"
assert_address_count veth-addr0 0

# MTU must still be 1400 (only addresses were removed, not the MTU setting).
assert_mtu veth-addr0 1400

echo "PASS: 600-e2e-addr-removal"
