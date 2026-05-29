#!/bin/bash
# 351-journal-seq.sh -- Standalone apply produces monotonically increasing sequence numbers.
#
# Tests .seq file persistence across two separate netfyr apply invocations
# in daemon-free mode. No daemon is started.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/351-journal-seq.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 351-journal-seq: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 351-journal-seq: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

JOURNAL_DIR="$TMPDIR_TEST/journal"
FAKE_SOCKET="$TMPDIR_TEST/no-daemon.sock"
mkdir -p "$JOURNAL_DIR"

create_veth veth-e2e0 veth-e2e1

# ── Apply policy A (mtu=1400) ────────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-seq-a
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
    echo "FAIL: 351-journal-seq: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Apply policy B (mtu=1300) ────────────────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-seq-b
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
    echo "FAIL: 351-journal-seq: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Verify exactly 2 policy_apply entries with strictly increasing seqs ──────

APPLY_COUNT=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$APPLY_COUNT" -ne 2 ]]; then
    echo "FAIL: 351-journal-seq: expected 2 policy_apply entries, found $APPLY_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

SEQ_FIRST=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][0].seq' \
    "$JOURNAL_DIR/current.ndjson")
SEQ_SECOND=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][1].seq' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$SEQ_SECOND" -le "$SEQ_FIRST" ]]; then
    echo "FAIL: 351-journal-seq: seq numbers must be strictly increasing: first=$SEQ_FIRST second=$SEQ_SECOND" >&2
    exit 1
fi

# Timestamps must be non-decreasing (ISO 8601 sorts lexicographically).
TS_FIRST=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][0].timestamp' \
    "$JOURNAL_DIR/current.ndjson")
TS_SECOND=$(jq -rs '[.[] | select(.trigger.type == "policy_apply")][1].timestamp' \
    "$JOURNAL_DIR/current.ndjson")

if [[ "$TS_SECOND" < "$TS_FIRST" ]]; then
    echo "FAIL: 351-journal-seq: second timestamp is earlier than first: $TS_FIRST > $TS_SECOND" >&2
    exit 1
fi

echo "PASS: 351-journal-seq"
