#!/bin/bash
# 600-e2e-history-show.sh -- End-to-end: netfyr history --show displays full entry detail.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-show.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-show: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-show: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-show: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-history-show: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-show: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a static policy: mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-history-show
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
    echo "FAIL: 600-e2e-history-show: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Extract the seq number of the policy_apply entry from the journal.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: 600-e2e-history-show: could not find policy_apply entry in journal" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Run history --show <seq> and capture the output.
SHOW_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# Verify output contains "Trigger:" and "policy-apply".
if ! echo "$SHOW_OUTPUT" | grep -q "Trigger:"; then
    echo "FAIL: 600-e2e-history-show: output does not contain 'Trigger:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "policy-apply\|policy_apply"; then
    echo "FAIL: 600-e2e-history-show: output does not mention 'policy-apply'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify output contains "Diff:" and "mtu".
if ! echo "$SHOW_OUTPUT" | grep -q "Diff:"; then
    echo "FAIL: 600-e2e-history-show: output does not contain 'Diff:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "mtu"; then
    echo "FAIL: 600-e2e-history-show: output does not mention 'mtu'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify output contains "Outcome:" and "applied".
if ! echo "$SHOW_OUTPUT" | grep -q "Outcome:"; then
    echo "FAIL: 600-e2e-history-show: output does not contain 'Outcome:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "applied"; then
    echo "FAIL: 600-e2e-history-show: output does not mention 'applied'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify output contains "State after:" section header.
if ! echo "$SHOW_OUTPUT" | grep -qi "State after:"; then
    echo "FAIL: 600-e2e-history-show: output does not contain 'State after:' section" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify State after section contains YAML sequence element with type field.
if ! echo "$SHOW_OUTPUT" | grep -qe "- type: ethernet"; then
    echo "FAIL: 600-e2e-history-show: State after section does not contain '- type: ethernet'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify State after section contains the interface name.
if ! echo "$SHOW_OUTPUT" | grep -q "name: veth-e2e0"; then
    echo "FAIL: 600-e2e-history-show: State after section does not contain 'name: veth-e2e0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify State after section contains mtu value.
if ! echo "$SHOW_OUTPUT" | grep -q "mtu: 1400"; then
    echo "FAIL: 600-e2e-history-show: State after section does not contain 'mtu: 1400'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify output does NOT contain JSON-style inline arrays.
if echo "$SHOW_OUTPUT" | grep -q '\["'; then
    echo "FAIL: 600-e2e-history-show: output contains JSON inline array syntax '[\"'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify output does NOT contain JSON-style inline objects.
if echo "$SHOW_OUTPUT" | grep -q '{"'; then
    echo "FAIL: 600-e2e-history-show: output contains JSON inline object syntax '{\"'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-show"
