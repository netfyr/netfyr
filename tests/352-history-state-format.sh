#!/bin/bash
# 352-history-state-format.sh -- History --show state-after format matches netfyr query output.
#
# Spec test 19b: state-after section uses YAML block-style sequences, not JSON arrays.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 352-history-state-format: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
add_address veth-e2e0 10.99.0.1/24
start_daemon

# Apply a policy with mtu and an address.
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
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-state-format: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Capture query output.
QUERY_OUTPUT=$("$NETFYR_BIN" query -s name=veth-e2e0 2>&1)

# Find policy_apply entry seq.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: 352-history-state-format: could not find policy_apply entry" >&2
    exit 1
fi

SHOW_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# Extract state-after section.
STATE_AFTER=$(echo "$SHOW_OUTPUT" | sed -n '/[Ss]tate after:/,$ p' | tail -n +2)
if [[ -z "$STATE_AFTER" ]]; then
    echo "FAIL: 352-history-state-format: could not find 'State after:' section" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify state-after has the same structural fields as query output.
for field in "type: ethernet" "name: veth-e2e0" "mtu: 1400"; do
    if ! echo "$STATE_AFTER" | grep -q "$field"; then
        echo "FAIL: 352-history-state-format: State after missing '$field'" >&2
        echo "      state_after: $STATE_AFTER" >&2
        exit 1
    fi
done

# Verify addresses appear as YAML block sequence, not JSON inline array.
if ! echo "$STATE_AFTER" | grep -q '^\s*- 10\.99\.0\.1/24'; then
    echo "FAIL: 352-history-state-format: addresses not in YAML block sequence format" >&2
    echo "      expected '- 10.99.0.1/24' on its own line" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi
if echo "$STATE_AFTER" | grep -q '\['; then
    echo "FAIL: 352-history-state-format: State after contains JSON-style inline array '[...]'" >&2
    echo "      state_after: $STATE_AFTER" >&2
    exit 1
fi

# Verify query output also uses block sequence for addresses.
if ! echo "$QUERY_OUTPUT" | grep -q '^\s*- 10\.99\.0\.1/24'; then
    echo "FAIL: 352-history-state-format: query output does not use YAML block sequence" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-state-format"
