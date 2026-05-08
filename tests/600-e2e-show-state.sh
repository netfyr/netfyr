#!/bin/bash
# 600-e2e-show-state.sh -- End-to-end: netfyr show displays State and Addresses lines.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-state: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-state: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

# Create a veth pair and assign an address to the managed endpoint.
create_veth veth-e2e0 veth-e2e1
add_address veth-e2e0 10.55.0.1/24

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-state: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-state: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a static policy with an address.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-show-state
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - 10.55.0.1/24
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-state: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run netfyr show and capture output (disable colors for reliable grep).
SHOW_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Verify State line appears for the managed interface.
if ! echo "$SHOW_OUTPUT" | grep -q "State:"; then
    echo "FAIL: 600-e2e-show-state: show output does not contain 'State:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify State shows "up" (veth pairs are up by default after create_veth).
if ! echo "$SHOW_OUTPUT" | grep -q "up"; then
    echo "FAIL: 600-e2e-show-state: show output does not contain 'up' in State line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Addresses line shows the configured address.
if ! echo "$SHOW_OUTPUT" | grep -q "Addresses:.*10.55.0.1/24"; then
    echo "FAIL: 600-e2e-show-state: show output does not contain 'Addresses:.*10.55.0.1/24'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-state"
