#!/bin/bash
# 600-e2e-journal-apply.sh -- End-to-end: netfyr apply records a correct journal entry.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-journal-apply.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-journal-apply: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-journal-apply: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-journal-apply: 'jq' not found; install jq to run this test" >&2
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

# Start the daemon with the configured journal directory.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-journal-apply: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-journal-apply: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a static policy: mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-journal-apply
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
    echo "FAIL: 600-e2e-journal-apply: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify the journal file was created.
if [[ ! -f "$JOURNAL_DIR/current.ndjson" ]]; then
    echo "FAIL: 600-e2e-journal-apply: current.ndjson not found in journal directory" >&2
    exit 1
fi

# Extract the last policy_apply entry from the ndjson file.
APPLY_ENTRY=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$APPLY_ENTRY" == "null" || -z "$APPLY_ENTRY" ]]; then
    echo "FAIL: 600-e2e-journal-apply: no policy_apply entry found in journal" >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify trigger.type == "policy_apply".
TRIGGER_TYPE=$(echo "$APPLY_ENTRY" | jq -r '.trigger.type')
if [[ "$TRIGGER_TYPE" != "policy_apply" ]]; then
    echo "FAIL: 600-e2e-journal-apply: expected trigger.type=policy_apply, got $TRIGGER_TYPE" >&2
    exit 1
fi

# Verify diff has an operation for veth-e2e0 with an mtu field change.
MTU_CHANGE_COUNT=$(echo "$APPLY_ENTRY" | jq '
    [.diff.operations[] |
     select(.entity_name == "veth-e2e0") |
     .field_changes[] |
     select(.field_name == "mtu")] | length')
if [[ "$MTU_CHANGE_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-journal-apply: diff does not contain an mtu field change for veth-e2e0" >&2
    echo "      diff: $(echo "$APPLY_ENTRY" | jq '.diff')" >&2
    exit 1
fi

# Verify state_after contains veth-e2e0 with mtu=1400.
MTU_AFTER=$(echo "$APPLY_ENTRY" | jq '.state_after.entities[] |
    select(.selector_name == "veth-e2e0") | .fields.mtu')
if [[ "$MTU_AFTER" != "1400" ]]; then
    echo "FAIL: 600-e2e-journal-apply: state_after.mtu expected 1400, got $MTU_AFTER" >&2
    echo "      state_after: $(echo "$APPLY_ENTRY" | jq '.state_after')" >&2
    exit 1
fi

# Verify outcome.kind == "applied" and succeeded >= 1.
OUTCOME_KIND=$(echo "$APPLY_ENTRY" | jq -r '.outcome.kind')
OUTCOME_SUCCEEDED=$(echo "$APPLY_ENTRY" | jq -r '.outcome.succeeded')
if [[ "$OUTCOME_KIND" != "applied" ]]; then
    echo "FAIL: 600-e2e-journal-apply: expected outcome.kind=applied, got $OUTCOME_KIND" >&2
    exit 1
fi
if [[ "$OUTCOME_SUCCEEDED" -lt 1 ]]; then
    echo "FAIL: 600-e2e-journal-apply: expected outcome.succeeded >= 1, got $OUTCOME_SUCCEEDED" >&2
    exit 1
fi

echo "PASS: 600-e2e-journal-apply"
