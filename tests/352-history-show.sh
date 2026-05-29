#!/bin/bash
# 352-history-show.sh -- History --show displays full entry detail with correct format.
#
# Spec test 19: netfyr history --show <seq> shows trigger, diff, outcome, state-after.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 352-history-show: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# Apply a policy setting mtu=1400 on veth-e2e0.
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
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-show: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Extract the seq of the policy_apply entry.
APPLY_SEQ=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$APPLY_SEQ" || "$APPLY_SEQ" == "null" ]]; then
    echo "FAIL: 352-history-show: could not find policy_apply entry in journal" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

SHOW_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history --show "$APPLY_SEQ" 2>&1)

# Verify Trigger and policy-apply.
if ! echo "$SHOW_OUTPUT" | grep -q "Trigger:"; then
    echo "FAIL: 352-history-show: output does not contain 'Trigger:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "policy-apply\|policy_apply"; then
    echo "FAIL: 352-history-show: output does not mention policy-apply trigger" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Diff and mtu.
if ! echo "$SHOW_OUTPUT" | grep -q "Diff:"; then
    echo "FAIL: 352-history-show: output does not contain 'Diff:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "mtu"; then
    echo "FAIL: 352-history-show: output does not mention mtu in diff" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify Outcome.
if ! echo "$SHOW_OUTPUT" | grep -q "Outcome:"; then
    echo "FAIL: 352-history-show: output does not contain 'Outcome:'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -qi "applied"; then
    echo "FAIL: 352-history-show: output does not mention 'applied' in outcome" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify State after section.
if ! echo "$SHOW_OUTPUT" | grep -qi "State after:"; then
    echo "FAIL: 352-history-show: output does not contain 'State after:' section" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "- type: ethernet"; then
    echo "FAIL: 352-history-show: State after does not contain '- type: ethernet'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "name: veth-e2e0"; then
    echo "FAIL: 352-history-show: State after does not contain 'name: veth-e2e0'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if ! echo "$SHOW_OUTPUT" | grep -q "mtu: 1400"; then
    echo "FAIL: 352-history-show: State after does not contain 'mtu: 1400'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Verify no JSON inline arrays or objects in output.
if echo "$SHOW_OUTPUT" | grep -q '\["'; then
    echo "FAIL: 352-history-show: output contains JSON inline array '[\"'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi
if echo "$SHOW_OUTPUT" | grep -q '{"'; then
    echo "FAIL: 352-history-show: output contains JSON inline object '{\"'" >&2
    echo "      output: $SHOW_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-show"
