#!/bin/bash
# 352-history-json.sh -- History -o json produces valid JSON array with required fields.
#
# Spec test 20: netfyr history -n 5 -o json outputs a JSON array.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 352-history-json: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# Apply policy A.
POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-json-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-json: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Apply policy B.
POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-json-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-json: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run history -o json and pipe through jq to validate.
HISTORY_JSON=$("$NETFYR_BIN" history -n 10 -o json --trigger apply 2>&1)

IS_ARRAY=$(echo "$HISTORY_JSON" | jq 'type == "array"' 2>/dev/null || echo "false")
if [[ "$IS_ARRAY" != "true" ]]; then
    echo "FAIL: 352-history-json: output is not a valid JSON array" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

# Verify exactly 2 policy_apply entries.
APPLY_COUNT=$(echo "$HISTORY_JSON" | jq '[.[] | select(.trigger.type == "policy_apply")] | length')
if [[ "$APPLY_COUNT" -ne 2 ]]; then
    echo "FAIL: 352-history-json: expected 2 policy_apply entries, found $APPLY_COUNT" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

# Verify each element has required fields.
ALL_HAVE_FIELDS=$(echo "$HISTORY_JSON" | jq '
    all(
        has("seq") and
        has("timestamp") and
        has("trigger") and
        has("outcome")
    )')
if [[ "$ALL_HAVE_FIELDS" != "true" ]]; then
    echo "FAIL: 352-history-json: not all elements have seq, timestamp, trigger, outcome" >&2
    echo "      output: $HISTORY_JSON" >&2
    exit 1
fi

echo "PASS: 352-history-json"
