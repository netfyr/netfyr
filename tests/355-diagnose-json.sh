#!/bin/bash
# 355-diagnose-json.sh -- End-to-end: netfyr diagnose -o json produces valid JSON.
#
# Spec test 32: Verifies that `netfyr diagnose -o json` produces a valid JSON
# array with the required fields when drift is detected.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 355-diagnose-json: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# Apply a policy setting mtu=1400 on veth-e2e0.
cat > "$POLICY_DIR/policy.yaml" <<'EOF'
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
"$NETFYR_BIN" apply "$POLICY_DIR/policy.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 355-diagnose-json: apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Change mtu externally to 1500 (causes drift).
ip link set veth-e2e0 mtu 1500

# Wait for debounce.
sleep 1

# Run diagnose with JSON output.
DIAGNOSE_JSON=""
DIAGNOSE_EXIT=0
DIAGNOSE_JSON=$("$NETFYR_BIN" diagnose -o json 2>&1) || DIAGNOSE_EXIT=$?

# Exit code should be 1 (warning) due to drift.
if [[ $DIAGNOSE_EXIT -ne 1 ]]; then
    echo "FAIL: 355-diagnose-json: expected exit code 1, got $DIAGNOSE_EXIT" >&2
    echo "      output: $DIAGNOSE_JSON" >&2
    exit 1
fi

# Validate it is a JSON array.
IS_ARRAY=$(echo "$DIAGNOSE_JSON" | jq 'type == "array"' 2>/dev/null || echo "false")
if [[ "$IS_ARRAY" != "true" ]]; then
    echo "FAIL: 355-diagnose-json: output is not a valid JSON array" >&2
    echo "      output: $DIAGNOSE_JSON" >&2
    exit 1
fi

# Verify there is an element with "pattern": "configuration_drift".
DRIFT_ELEMENT=$(echo "$DIAGNOSE_JSON" | jq '[.[] | select(.pattern == "configuration_drift")] | length')
if [[ "$DRIFT_ELEMENT" -lt 1 ]]; then
    echo "FAIL: 355-diagnose-json: no element with pattern=configuration_drift" >&2
    echo "      output: $DIAGNOSE_JSON" >&2
    exit 1
fi

# Verify the drift element has all required fields.
HAS_FIELDS=$(echo "$DIAGNOSE_JSON" | jq '
    [.[] | select(.pattern == "configuration_drift")] |
    .[0] |
    (has("entity") and
     has("severity") and
     has("summary") and
     has("details") and
     has("suggested_actions") and
     has("related_entries"))')
if [[ "$HAS_FIELDS" != "true" ]]; then
    echo "FAIL: 355-diagnose-json: drift element missing required fields" >&2
    echo "      output: $DIAGNOSE_JSON" >&2
    exit 1
fi

echo "PASS: 355-diagnose-json"
