#!/bin/bash
# 600-e2e-show-json-fields.sh -- End-to-end: netfyr show -o json includes enabled, carrier,
# addresses, config_state, and config_drift fields.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-fields: jq not found; install jq to run JSON tests" >&2
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
add_address veth-e2e0 10.77.0.1/24

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-show-json-fields: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-show-json-fields: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply a static policy with address and mtu.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-show-json-fields
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - 10.77.0.1/24
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-json-fields: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Phase 1: Synced state — JSON has enabled, carrier, addresses, config_state ─

JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)

if ! echo "$JSON_OUTPUT" | jq . >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-fields: show -o json output is not valid JSON" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

MANAGED=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-e2e0")')
if [[ -z "$MANAGED" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: veth-e2e0 not found in interfaces" >&2
    echo "      JSON: $JSON_OUTPUT" >&2
    exit 1
fi

# Verify enabled is a boolean.
ENABLED=$(echo "$MANAGED" | jq '.enabled')
if [[ "$ENABLED" != "true" && "$ENABLED" != "false" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: enabled is '$ENABLED', expected true or false" >&2
    exit 1
fi

# Verify carrier is a boolean.
CARRIER=$(echo "$MANAGED" | jq '.carrier')
if [[ "$CARRIER" != "true" && "$CARRIER" != "false" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: carrier is '$CARRIER', expected true or false" >&2
    exit 1
fi

# Verify addresses contains the assigned IP.
if ! echo "$MANAGED" | jq -e '.addresses[] | select(startswith("10.77.0.1"))' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-fields: addresses does not contain '10.77.0.1'" >&2
    echo "      managed: $MANAGED" >&2
    exit 1
fi

# Verify config_state is "applied".
CONFIG_STATE=$(echo "$MANAGED" | jq -r '.config_state')
if [[ "$CONFIG_STATE" != "applied" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: config_state is '$CONFIG_STATE', expected 'applied'" >&2
    exit 1
fi

# Verify config_drift is absent when applied.
if echo "$MANAGED" | jq -e '.config_drift' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-fields: config_drift should be absent when applied" >&2
    exit 1
fi

# Verify unmanaged interface has no config_state.
UNMANAGED=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-e2e1")')
if echo "$UNMANAGED" | jq -e '.config_state' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-fields: unmanaged veth-e2e1 should not have config_state" >&2
    exit 1
fi

# ── Phase 2: Create drift — config_state becomes "drifted" with config_drift ─

ip link set veth-e2e0 mtu 1500
sleep 1

JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1)
MANAGED=$(echo "$JSON_OUTPUT" | jq '.interfaces[] | select(.name == "veth-e2e0")')

CONFIG_STATE=$(echo "$MANAGED" | jq -r '.config_state')
if [[ "$CONFIG_STATE" != "drifted" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: config_state is '$CONFIG_STATE' after MTU change, expected 'drifted'" >&2
    exit 1
fi

# Verify config_drift is present and contains the mtu field.
DRIFT_COUNT=$(echo "$MANAGED" | jq '.config_drift | length')
if [[ "$DRIFT_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-show-json-fields: config_drift is empty, expected at least 1 entry" >&2
    exit 1
fi

MTU_DRIFT=$(echo "$MANAGED" | jq '.config_drift[] | select(.field_name == "mtu")')
if [[ -z "$MTU_DRIFT" ]]; then
    echo "FAIL: 600-e2e-show-json-fields: config_drift does not contain mtu field" >&2
    echo "      config_drift: $(echo "$MANAGED" | jq '.config_drift')" >&2
    exit 1
fi

# Verify drift description mentions both expected and actual values.
DRIFT_DESC=$(echo "$MTU_DRIFT" | jq -r '.description')
if ! echo "$DRIFT_DESC" | grep -q "1400"; then
    echo "FAIL: 600-e2e-show-json-fields: drift description does not mention expected value 1400" >&2
    echo "      description: $DRIFT_DESC" >&2
    exit 1
fi
if ! echo "$DRIFT_DESC" | grep -q "1500"; then
    echo "FAIL: 600-e2e-show-json-fields: drift description does not mention actual value 1500" >&2
    echo "      description: $DRIFT_DESC" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-json-fields"
