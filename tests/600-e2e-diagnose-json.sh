#!/bin/bash
# 600-e2e-diagnose-json.sh -- End-to-end: diagnose -o json produces valid structured output.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-diagnose-json.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-diagnose-json: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-diagnose-json: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-diagnose-json: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

create_veth veth-e2e0 veth-e2e1

# Start the daemon with a temp journal directory.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-diagnose-json: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-diagnose-json: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write and apply a static policy: mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-diagnose-json
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-diagnose-json: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Externally change mtu to 1500 to create drift.
ip link set veth-e2e0 mtu 1500

# Wait for debounce window to expire.
sleep 1

# Run diagnose -o json; capture output.
DIAGNOSE_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" diagnose -o json 2>&1) || true

# Verify: output is a valid JSON array.
JSON_TYPE=$(echo "$DIAGNOSE_OUTPUT" | jq -r 'type' 2>/dev/null) || true
if [[ "$JSON_TYPE" != "array" ]]; then
    echo "FAIL: 600-e2e-diagnose-json: output is not a valid JSON array (type=$JSON_TYPE)" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Extract the configuration_drift finding.
DRIFT_FINDING=$(echo "$DIAGNOSE_OUTPUT" | jq '.[] | select(.pattern == "configuration_drift")' 2>/dev/null) || true
if [[ -z "$DRIFT_FINDING" || "$DRIFT_FINDING" == "null" ]]; then
    echo "FAIL: 600-e2e-diagnose-json: no 'configuration_drift' finding in JSON output" >&2
    echo "      output: $DIAGNOSE_OUTPUT" >&2
    exit 1
fi

# Verify all required fields are present in the drift finding.
check_field() {
    local field="$1"
    local has_field
    has_field=$(echo "$DRIFT_FINDING" | jq --arg f "$field" 'has($f)')
    if [[ "$has_field" != "true" ]]; then
        echo "FAIL: 600-e2e-diagnose-json: drift finding is missing required field '$field'" >&2
        echo "      finding: $DRIFT_FINDING" >&2
        exit 1
    fi
}

check_field "entity"
check_field "entity_type"
check_field "severity"
check_field "summary"
check_field "details"
check_field "suggested_actions"
check_field "related_entries"

echo "PASS: 600-e2e-diagnose-json"
