#!/bin/bash
# 600-e2e-route-idempotent.sh -- End-to-end: re-applying the same route policy
# does not produce a spurious diff and does not delete the route.
#
# Reproduces two bugs:
#   1. dry-run shows a route diff on second apply because the kernel-assigned
#      metric (100) makes the actual route map differ from the desired one.
#   2. Applying the same policy a second time deletes the default route.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-route-idempotent.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-route-idempotent: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-route-idempotent: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
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

create_veth veth-rt0 veth-rt1

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-route-idempotent: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-route-idempotent: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a static policy with an address and a default route (no metric).
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-route-idempotent
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  enabled: true
  addresses:
    - "10.99.0.50/24"
  routes:
    - destination: "0.0.0.0/0"
      gateway: "10.99.0.254"
EOF

# ── First apply ───────────────────────────────────────────────────────────────

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-route-idempotent: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify the route exists after first apply.
ROUTE_OUTPUT=$(ip route 2>&1)
if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.0.254"; then
    echo "FAIL: 600-e2e-route-idempotent: default route not present after first apply" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

# ── Dry-run must report no changes ────────────────────────────────────────────

DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1) \
    || DRY_RUN_EXIT=$?

# Exit code 0 means "no changes"; exit code 1 means "changes pending".
if [[ $DRY_RUN_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-route-idempotent: dry-run reports changes on second apply (exit=$DRY_RUN_EXIT)" >&2
    echo "      dry-run output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# ── Second apply must be a no-op ──────────────────────────────────────────────

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-route-idempotent: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# The default route must still be present after the second apply.
ROUTE_OUTPUT=$(ip route 2>&1)
if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.0.254"; then
    echo "FAIL: 600-e2e-route-idempotent: default route disappeared after second apply" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-route-idempotent"
