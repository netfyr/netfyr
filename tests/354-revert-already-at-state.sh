#!/bin/bash
# 354-revert-already-at-state.sh -- Revert when system already matches target state.
#
# AC: Revert when already at target state → output "No changes needed", exit 0.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/354-revert-already-at-state.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 354-revert-already-at-state: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 354-revert-already-at-state: 'jq' not found; install jq to run this test" >&2
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

# Apply policy A: mtu=1400.
POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-revert-same-a
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
    echo "FAIL: 354-revert-already-at-state: apply (mtu=1400) exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Confirm mtu=1400.
assert_mtu veth-e2e0 1400

# AC: run revert to entry #1 — system already has mtu=1400, so no changes needed.
REVERT_OUTPUT=""
REVERT_EXIT=0
REVERT_OUTPUT=$(NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert 1 2>&1) || REVERT_EXIT=$?

# AC: exit code is 0.
if [[ $REVERT_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert-already-at-state: expected exit code 0, got $REVERT_EXIT" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

# AC: the output shows "No changes needed".
if ! echo "$REVERT_OUTPUT" | grep -qi "no changes needed"; then
    echo "FAIL: 354-revert-already-at-state: output should say 'No changes needed'" >&2
    echo "      output: $REVERT_OUTPUT" >&2
    exit 1
fi

# Confirm no revert entry was written to the journal (no changes → no journal write).
REVERT_COUNT=$(jq -rs '[.[] | select(.trigger.type == "revert")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$REVERT_COUNT" -ne 0 ]]; then
    echo "FAIL: 354-revert-already-at-state: no-op revert must not write a journal entry" >&2
    exit 1
fi

echo "PASS: 354-revert-already-at-state"
