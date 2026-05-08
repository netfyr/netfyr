#!/bin/bash
# 600-e2e-show-config-drift.sh -- End-to-end: netfyr show displays Config applied/drifted status.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-config-drift: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-config-drift: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

create_veth veth-e2e0 veth-e2e1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-config-drift: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-config-drift: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a static policy: mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-show-drift
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
    echo "FAIL: 600-e2e-show-config-drift: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# ── Phase 1: Config should be applied right after apply ────────────────────

SHOW_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

if ! echo "$SHOW_OUTPUT" | grep -q "Config:.*applied"; then
    echo "FAIL: 600-e2e-show-config-drift: show output does not contain 'Config:.*applied' after apply" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify unmanaged interfaces do NOT show Config line.
# veth-e2e1 is unmanaged (no policy). Extract lines after veth-e2e1 up to
# the next interface or end of output.
if echo "$SHOW_OUTPUT" | sed -n '/veth-e2e1/,/^  [^ ]/p' | grep -q "Config:"; then
    echo "FAIL: 600-e2e-show-config-drift: unmanaged veth-e2e1 unexpectedly has Config line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# ── Phase 2: Externally change MTU to create drift ───────────────────────

ip link set veth-e2e0 mtu 1500

# Wait for debounce window.
sleep 1

SHOW_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

if ! echo "$SHOW_OUTPUT" | grep -q "Config:.*drifted"; then
    echo "FAIL: 600-e2e-show-config-drift: show output does not contain 'Config:.*drifted' after MTU change" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify drift details mention the expected and actual MTU values.
if ! echo "$SHOW_OUTPUT" | grep -q "mtu"; then
    echo "FAIL: 600-e2e-show-config-drift: drift details do not mention 'mtu'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUTPUT" | grep -q "1400"; then
    echo "FAIL: 600-e2e-show-config-drift: drift details do not mention expected value 1400" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUTPUT" | grep -q "1500"; then
    echo "FAIL: 600-e2e-show-config-drift: drift details do not mention actual value 1500" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-config-drift"
