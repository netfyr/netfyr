#!/bin/bash
# 352-history-list.sh -- History list output: columns, ordering, trigger, entity display, timestamps.
#
# Spec test 18: netfyr history -n shows entries with correct columns and formatting.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# ── Apply policy A (mtu=1400, address 10.99.0.1/24) ─────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: veth-e2e0-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-list: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300, no address) ───────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: veth-e2e0-b
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
    echo "FAIL: 352-history-list: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Run history and verify output ────────────────────────────────────────────

HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 10 2>&1)

# Verify header contains all required columns and no OUTCOME column.
HEADER=$(echo "$HISTORY_OUTPUT" | head -n 1)
for col in SEQ TIMESTAMP TRIGGER ENTITIES CHANGES; do
    if ! echo "$HEADER" | grep -q "$col"; then
        echo "FAIL: 352-history-list: header missing column '$col'" >&2
        echo "      header: $HEADER" >&2
        exit 1
    fi
done
if echo "$HEADER" | grep -q "OUTCOME"; then
    echo "FAIL: 352-history-list: header must not contain 'OUTCOME' column" >&2
    echo "      header: $HEADER" >&2
    exit 1
fi

# Verify at least 2 apply rows.
APPLY_COUNT=$(echo "$HISTORY_OUTPUT" | grep -c "apply (" || true)
if [[ "$APPLY_COUNT" -lt 2 ]]; then
    echo "FAIL: 352-history-list: expected >= 2 apply rows, got $APPLY_COUNT" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify reverse chronological order.
SEQ_FIRST=$(echo "$HISTORY_OUTPUT" | grep "apply (" | head -n 1 | awk '{print $1}')
SEQ_SECOND=$(echo "$HISTORY_OUTPUT" | grep "apply (" | sed -n '2p' | awk '{print $1}')
if [[ "$SEQ_FIRST" -le "$SEQ_SECOND" ]]; then
    echo "FAIL: 352-history-list: entries not in reverse order (first=$SEQ_FIRST, second=$SEQ_SECOND)" >&2
    exit 1
fi

# Verify TRIGGER shows "apply (veth-e2e0-b)" for most recent entry.
FIRST_APPLY=$(echo "$HISTORY_OUTPUT" | grep "apply (" | head -n 1)
if ! echo "$FIRST_APPLY" | grep -qF "apply (veth-e2e0-b)"; then
    echo "FAIL: 352-history-list: most recent TRIGGER should show 'apply (veth-e2e0-b)'" >&2
    echo "      line: $FIRST_APPLY" >&2
    exit 1
fi

# Verify ENTITIES shows "veth-e2e0" without +/- prefix (entity was modified).
if ! echo "$HISTORY_OUTPUT" | grep -qF "veth-e2e0"; then
    echo "FAIL: 352-history-list: output should contain 'veth-e2e0'" >&2
    exit 1
fi
if echo "$HISTORY_OUTPUT" | grep -qF "+veth-e2e0"; then
    echo "FAIL: 352-history-list: ENTITIES should not show '+veth-e2e0' (entity was modified, not created)" >&2
    exit 1
fi

# Verify CHANGES column for the most recent apply shows MTU change values.
if ! echo "$FIRST_APPLY" | grep -qF "1400"; then
    echo "FAIL: 352-history-list: CHANGES column should contain old mtu '1400'" >&2
    echo "      line: $FIRST_APPLY" >&2
    exit 1
fi
if ! echo "$FIRST_APPLY" | grep -qF "1300"; then
    echo "FAIL: 352-history-list: CHANGES column should contain new mtu '1300'" >&2
    echo "      line: $FIRST_APPLY" >&2
    exit 1
fi

# Verify CHANGES column shows address removal by value.
if ! echo "$FIRST_APPLY" | grep -qF "10.99.0.1"; then
    echo "FAIL: 352-history-list: CHANGES column should show address value '10.99.0.1'" >&2
    echo "      line: $FIRST_APPLY" >&2
    exit 1
fi

# Verify TIMESTAMP uses relative format (entries are recent).
DATA_ROW=$(echo "$HISTORY_OUTPUT" | grep "apply (" | head -n 1)
if ! echo "$DATA_ROW" | grep -qE "ago|just now"; then
    echo "FAIL: 352-history-list: TIMESTAMP should use relative format (e.g. 'N min ago')" >&2
    echo "      line: $DATA_ROW" >&2
    exit 1
fi

# ── Verify --absolute-timestamps shows YYYY-MM-DD pattern ───────────────────

ABS_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history --absolute-timestamps -n 5 2>&1)
if ! echo "$ABS_OUTPUT" | grep -qE "[0-9]{4}-[0-9]{2}-[0-9]{2}"; then
    echo "FAIL: 352-history-list: --absolute-timestamps should show YYYY-MM-DD pattern" >&2
    echo "      output: $ABS_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-list"
