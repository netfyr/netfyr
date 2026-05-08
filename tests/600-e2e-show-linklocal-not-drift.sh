#!/bin/bash
# 600-e2e-show-linklocal-not-drift.sh -- End-to-end: kernel-assigned link-local
# IPv6 addresses must not be reported as configuration drift.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: jq not found; install jq to run JSON tests" >&2
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
add_address veth-e2e0 fd00::1/64
add_address veth-e2e0 10.77.0.1/24

# Verify that the kernel assigned a link-local address.
if ! ip addr show veth-e2e0 | grep -q "fe80::"; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: kernel did not assign fe80:: link-local address" >&2
    exit 1
fi

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-linklocal-not-drift: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-linklocal-not-drift: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a policy with static IPv4 + IPv6 addresses (no link-local).
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-linklocal
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - 10.77.0.1/24
    - fd00::1/64
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Phase 1: Text output — Config must be "applied" despite fe80:: present ─

SHOW_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show 2>&1)

if ! echo "$SHOW_OUTPUT" | grep -q "Config:.*applied"; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: show output does not contain 'Config:.*applied'" >&2
    echo "      Link-local fe80:: address may be causing false drift" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

if echo "$SHOW_OUTPUT" | grep -q "Config:.*drifted"; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: show reports 'drifted' due to link-local address" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# ── Phase 2: JSON output — config_state must be "applied" ─────────────────

JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)

MANAGED=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-e2e0")')
if [[ -z "$MANAGED" ]]; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: veth-e2e0 not found in JSON output" >&2
    exit 1
fi

CONFIG_STATE=$(echo "$MANAGED" | jq -r '.config_state')
if [[ "$CONFIG_STATE" != "applied" ]]; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: config_state is '$CONFIG_STATE', expected 'applied'" >&2
    echo "      Link-local fe80:: address should not cause drift" >&2
    echo "      managed: $MANAGED" >&2
    exit 1
fi

if echo "$MANAGED" | jq -e '.config_drift' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-linklocal-not-drift: config_drift should be absent when applied" >&2
    echo "      managed: $MANAGED" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-linklocal-not-drift"
