#!/bin/bash
# 600-e2e-show-static.sh -- End-to-end: netfyr show with static-only policies.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-static: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-static: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

# Create two separate veth pairs; manage the first endpoint of each.
create_veth veth-e2e0 veth-e2e1
create_veth veth-e2e2 veth-e2e3

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-static: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-static: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write two static policies into a directory.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/policy-a.yaml" <<'EOF'
kind: policy
name: e2e-show-static-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

cat > "$APPLY_DIR/policy-b.yaml" <<'EOF'
kind: policy
name: e2e-show-static-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e2
  mtu: 1300
EOF

# Apply both policies.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-static: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run netfyr show and capture output.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

# Verify Status: running.
if ! echo "$SHOW_OUTPUT" | grep -q "Status:  running"; then
    echo "FAIL: 600-e2e-show-static: show output does not contain 'Status:  running'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Uptime: line is present.
if ! echo "$SHOW_OUTPUT" | grep -q "Uptime:"; then
    echo "FAIL: 600-e2e-show-static: show output does not contain 'Uptime:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify both managed interfaces appear.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-e2e0"; then
    echo "FAIL: 600-e2e-show-static: show output does not contain 'veth-e2e0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "veth-e2e2"; then
    echo "FAIL: 600-e2e-show-static: show output does not contain 'veth-e2e2'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify at least 2 static policy lines appear.
STATIC_COUNT=$(echo "$SHOW_OUTPUT" | grep -c "(static)") || STATIC_COUNT=0
if [[ $STATIC_COUNT -lt 2 ]]; then
    echo "FAIL: 600-e2e-show-static: expected at least 2 '(static)' policy entries, got $STATIC_COUNT" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify no DHCP or Lease lines appear.
if echo "$SHOW_OUTPUT" | grep -q "DHCP:"; then
    echo "FAIL: 600-e2e-show-static: show output unexpectedly contains 'DHCP:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if echo "$SHOW_OUTPUT" | grep -q "Lease:"; then
    echo "FAIL: 600-e2e-show-static: show output unexpectedly contains 'Lease:' line" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-static"
