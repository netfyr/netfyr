#!/bin/bash
# 354-revert-dry-run.sh -- Revert --dry-run previews changes without applying.
#
# Spec test 23: netfyr revert --dry-run shows what would change, leaves state unchanged,
# and does not record a new journal entry.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/354-revert-dry-run.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 354-revert-dry-run: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 354-revert-dry-run: 'jq' not found; install jq to run this test" >&2
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
name: e2e-revert-dr-a
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
    echo "FAIL: 354-revert-dry-run: first apply (mtu=1400) exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B: mtu=1300 ─────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-revert-dr-b
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
    echo "FAIL: 354-revert-dry-run: second apply (mtu=1300) exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Confirm mtu is 1300.
assert_mtu veth-e2e0 1300

# Confirm exactly 2 entries before dry-run.
ENTRY_COUNT_BEFORE=$(jq -rs 'length' "$JOURNAL_DIR/current.ndjson")
if [[ "$ENTRY_COUNT_BEFORE" -ne 2 ]]; then
    echo "FAIL: 354-revert-dry-run: expected 2 journal entries before dry-run, got $ENTRY_COUNT_BEFORE" >&2
    exit 1
fi

# ── Run revert --dry-run ──────────────────────────────────────────────────────

DRY_RUN_OUTPUT=""
DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$(NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert 1 --dry-run 2>&1) || DRY_RUN_EXIT=$?
# dry-run exits 1 (changes pending) — that's OK; we just need the output.

# AC: the output mentions "mtu: 1300 -> 1400" (Unicode arrow or ASCII arrow).
if ! echo "$DRY_RUN_OUTPUT" | grep -qE "mtu[: ]+1300"; then
    echo "FAIL: 354-revert-dry-run: dry-run output should show old mtu '1300'" >&2
    echo "      output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi
if ! echo "$DRY_RUN_OUTPUT" | grep -qE "1400"; then
    echo "FAIL: 354-revert-dry-run: dry-run output should show new mtu '1400'" >&2
    echo "      output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# AC: veth-e2e0 still has mtu=1300 (unchanged).
assert_mtu veth-e2e0 1300

# AC: no new journal entry was created — still only 2 entries.
ENTRY_COUNT_AFTER=$(jq -rs 'length' "$JOURNAL_DIR/current.ndjson")
if [[ "$ENTRY_COUNT_AFTER" -ne 2 ]]; then
    echo "FAIL: 354-revert-dry-run: expected 2 journal entries after dry-run, got $ENTRY_COUNT_AFTER" >&2
    echo "      (dry-run must not create a new journal entry)" >&2
    exit 1
fi

# Verify no "revert" trigger entry exists.
REVERT_COUNT=$(jq -rs '[.[] | select(.trigger.type == "revert")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$REVERT_COUNT" -ne 0 ]]; then
    echo "FAIL: 354-revert-dry-run: dry-run must not create a revert journal entry" >&2
    exit 1
fi

echo "PASS: 354-revert-dry-run"
