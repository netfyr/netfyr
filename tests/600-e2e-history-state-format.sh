#!/bin/bash
# 600-e2e-history-state-format.sh -- End-to-end: history --show state-after format matches query output.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-state-format.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-state-format: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-state-format: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-state-format: 'jq' not found; install jq to run this test" >&2
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

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-history-state-format: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-state-format: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a static policy: mtu=1400 and one address on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-history-state-format
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - 10.99.0.1/24
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-history-state-format: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Capture netfyr query YAML output.
QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    "$NETFYR_BIN" query -s name=veth-e2e0 2>&1)

# Extract the seq number of the policy_apply entry from the journal.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: 600-e2e-history-state-format: could not find policy_apply entry in journal" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Run history --show and capture the output.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# Extract the "State after" section (everything after the "State after:" header line).
STATE_AFTER=$(echo "$SHOW_OUTPUT" | sed -n '/[Ss]tate after:/,$ p' | tail -n +2)

if [[ -z "$STATE_AFTER" ]]; then
    echo "FAIL: 600-e2e-history-state-format: could not find 'State after:' section in history output" >&2
    echo "      history output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify state-after contains the same structural fields as query output.
if ! echo "$STATE_AFTER" | grep -q "type: ethernet"; then
    echo "FAIL: 600-e2e-history-state-format: State after does not contain 'type: ethernet'" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi
if ! echo "$STATE_AFTER" | grep -q "name: veth-e2e0"; then
    echo "FAIL: 600-e2e-history-state-format: State after does not contain 'name: veth-e2e0'" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi
if ! echo "$STATE_AFTER" | grep -q "mtu: 1400"; then
    echo "FAIL: 600-e2e-history-state-format: State after does not contain 'mtu: 1400'" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi

# Verify query output also has the same structural fields (sanity check).
if ! echo "$QUERY_OUTPUT" | grep -q "type: ethernet"; then
    echo "FAIL: 600-e2e-history-state-format: query output does not contain 'type: ethernet'" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

# Verify addresses appear as YAML block sequence, not JSON inline array.
# Block sequence: "- 10.99.0.1/24" on its own line.
# JSON inline array: ["10.99.0.1/24"] -- must NOT appear.
if ! echo "$STATE_AFTER" | grep -q '^\s*- 10\.99\.0\.1/24'; then
    echo "FAIL: 600-e2e-history-state-format: addresses not in YAML block sequence format ('- 10.99.0.1/24')" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi
if echo "$STATE_AFTER" | grep -q '\['; then
    echo "FAIL: 600-e2e-history-state-format: State after contains JSON-style inline array '[...]'" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi

# Verify query output also uses block sequence for addresses.
if ! echo "$QUERY_OUTPUT" | grep -q '^\s*- 10\.99\.0\.1/24'; then
    echo "FAIL: 600-e2e-history-state-format: query output does not use YAML block sequence for addresses" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-state-format"
