#!/bin/bash
# 600-e2e-revert-dry-run.sh -- End-to-end: netfyr revert --dry-run previews without applying.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-revert-dry-run.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-revert-dry-run: 'jq' not found; install jq to run this test" >&2
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
        echo "FAIL: 600-e2e-revert-dry-run: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-revert-dry-run: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply policy A (mtu=1400) ────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-revert-dryrun-a
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
    "$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Extract the seq of the policy_apply entry for policy A.
SEQ_A=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | last | .seq' \
    "$JOURNAL_DIR/current.ndjson")
if [[ -z "$SEQ_A" || "$SEQ_A" == "null" ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: could not find policy_apply entry for policy A" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300) ────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-revert-dryrun-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1300

# ── Dry-run revert to state A ─────────────────────────────────────────────────

DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert "$SEQ_A" --dry-run 2>&1) || DRY_RUN_EXIT=$?

# Dry-run with pending changes should exit non-zero (1 = changes pending).
if [[ $DRY_RUN_EXIT -eq 0 ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: expected non-zero exit from dry-run with changes" >&2
    echo "      output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# Output must mention the mtu field.
if ! echo "$DRY_RUN_OUTPUT" | grep -qi "mtu"; then
    echo "FAIL: 600-e2e-revert-dry-run: dry-run output does not mention 'mtu'" >&2
    echo "      output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# MTU must remain 1300 — the dry-run must not apply any changes.
assert_mtu veth-e2e0 1300

# Verify no revert journal entry was created (dry-run should not write journal).
REVERT_COUNT=$(jq -rs '[.[] | select(.trigger.type == "revert")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$REVERT_COUNT" -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: dry-run unexpectedly created $REVERT_COUNT revert journal entries" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify still only 2 policy-apply entries (no extra entries written).
APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$APPLY_COUNT" -ne 2 ]]; then
    echo "FAIL: 600-e2e-revert-dry-run: expected 2 policy_apply entries, found $APPLY_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

echo "PASS: 600-e2e-revert-dry-run"
