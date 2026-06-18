#!/bin/bash
# 354-revert.sh -- Revert restores a previous network state (MTU change).
#
# Spec test 22: netfyr revert restores state from a previous journal entry.
# Verifies: MTU is restored, revert journal entry created, history shows revert trigger.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/354-revert.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 354-revert: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 354-revert: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

JOURNAL_DIR="$TMPDIR_TEST/journal"
# Point socket at a nonexistent path to force daemon-free mode.
FAKE_SOCKET="$TMPDIR_TEST/no-daemon.sock"
mkdir -p "$JOURNAL_DIR"

create_veth veth-e2e0 veth-e2e1

# ── Apply policy A: mtu=1400 ─────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-revert-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert: first apply (mtu=1400) exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Confirm first journal entry has seq=1.
SEQ1=$(jq -rs '.[0].seq' "$JOURNAL_DIR/current.ndjson")
if [[ "$SEQ1" != "1" ]]; then
    echo "FAIL: 354-revert: expected first journal entry to have seq=1, got $SEQ1" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Apply policy B: mtu=1300 ─────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-revert-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert: second apply (mtu=1300) exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify mtu is now 1300.
assert_mtu veth-e2e0 1300

# Confirm there are exactly 2 journal entries before revert.
ENTRY_COUNT_BEFORE=$(jq -rs 'length' "$JOURNAL_DIR/current.ndjson")
if [[ "$ENTRY_COUNT_BEFORE" -ne 2 ]]; then
    echo "FAIL: 354-revert: expected 2 journal entries before revert, got $ENTRY_COUNT_BEFORE" >&2
    exit 1
fi

# ── Run revert to entry #1 ───────────────────────────────────────────────────

REVERT_OUTPUT=""
REVERT_EXIT=0
REVERT_OUTPUT=$(NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert 1 2>&1) || REVERT_EXIT=$?
if [[ $REVERT_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert: revert exited with code $REVERT_EXIT" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

# AC: veth-e2e0 has mtu=1400.
assert_mtu veth-e2e0 1400

# AC: the output shows "Applied".
if ! echo "$REVERT_OUTPUT" | grep -qi "applied"; then
    echo "FAIL: 354-revert: revert output should mention 'Applied'" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

# AC: a journal entry with trigger "revert" and target_seq=1 is recorded.
REVERT_ENTRY=$(jq -rs '[.[] | select(.trigger.type == "revert")] | last' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$REVERT_ENTRY" == "null" || -z "$REVERT_ENTRY" ]]; then
    echo "FAIL: 354-revert: no revert entry found in journal" >&2
    echo "      journal contents:" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

TARGET_SEQ=$(echo "$REVERT_ENTRY" | jq -r '.trigger.target_seq')
if [[ "$TARGET_SEQ" != "1" ]]; then
    echo "FAIL: 354-revert: expected trigger.target_seq=1, got $TARGET_SEQ" >&2
    exit 1
fi

# Verify revert entry outcome is "applied".
OUTCOME_KIND=$(echo "$REVERT_ENTRY" | jq -r '.outcome.kind')
if [[ "$OUTCOME_KIND" != "applied" ]]; then
    echo "FAIL: 354-revert: revert entry outcome.kind expected 'applied', got $OUTCOME_KIND" >&2
    exit 1
fi

# Verify revert entry state_after reflects mtu=1400 (the target state).
MTU_AFTER=$(echo "$REVERT_ENTRY" | jq '.state_after.entities[] |
    select(.selector_name == "veth-e2e0") | .fields.mtu')
if [[ "$MTU_AFTER" != "1400" ]]; then
    echo "FAIL: 354-revert: revert entry state_after.mtu expected 1400, got $MTU_AFTER" >&2
    exit 1
fi

# AC: netfyr history -n 1 shows TRIGGER "revert (1)".
HISTORY_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 1 2>&1)
if ! echo "$HISTORY_OUTPUT" | grep -qF "revert (1)"; then
    echo "FAIL: 354-revert: history TRIGGER column should show 'revert (1)'" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 354-revert"
